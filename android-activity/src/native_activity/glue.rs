//! This 'glue' layer acts as an IPC shim between the JVM main thread and the Rust
//! main thread. Notifying Rust of lifecycle events from the JVM and handling
//! synchronization between the two threads.

use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{BufRead, BufReader},
    ops::Deref,
    os::unix::prelude::{FromRawFd, RawFd},
    ptr::{self, NonNull},
    sync::{Arc, Condvar, Mutex, Weak},
};

use libc;

use log::Level;
use ndk::{configuration::Configuration, input_queue::InputQueue, native_window::NativeWindow};
use ndk_sys::ANativeActivity;

use crate::ConfigurationRef;

use super::{AndroidApp, Rect};

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum AppCmd {
    InputQueueChanged = 0,
    InitWindow = 1,
    TermWindow = 2,
    WindowResized = 3,
    WindowRedrawNeeded = 4,
    ContentRectChanged = 5,
    GainedFocus = 6,
    LostFocus = 7,
    ConfigChanged = 8,
    LowMemory = 9,
    Start = 10,
    Resume = 11,
    SaveState = 12,
    Pause = 13,
    Stop = 14,
    Destroy = 15,
}
impl TryFrom<i8> for AppCmd {
    type Error = ();

    fn try_from(value: i8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(AppCmd::InputQueueChanged),
            1 => Ok(AppCmd::InitWindow),
            2 => Ok(AppCmd::TermWindow),
            3 => Ok(AppCmd::WindowResized),
            4 => Ok(AppCmd::WindowRedrawNeeded),
            5 => Ok(AppCmd::ContentRectChanged),
            6 => Ok(AppCmd::GainedFocus),
            7 => Ok(AppCmd::LostFocus),
            8 => Ok(AppCmd::ConfigChanged),
            9 => Ok(AppCmd::LowMemory),
            10 => Ok(AppCmd::Start),
            11 => Ok(AppCmd::Resume),
            12 => Ok(AppCmd::SaveState),
            13 => Ok(AppCmd::Pause),
            14 => Ok(AppCmd::Stop),
            15 => Ok(AppCmd::Destroy),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Default, Debug)]
pub enum State {
    #[default]
    Init,
    Start,
    Resume,
    Pause,
    Stop,
}

#[derive(Debug)]
pub struct WaitableNativeActivityState {
    pub activity: *mut ndk_sys::ANativeActivity,

    pub mutex: Mutex<NativeActivityState>,
    pub cond: Condvar,
}

#[derive(Debug, Clone)]
pub struct NativeActivityGlue {
    pub inner: Arc<WaitableNativeActivityState>,
}
unsafe impl Send for NativeActivityGlue {}
unsafe impl Sync for NativeActivityGlue {}

impl Deref for NativeActivityGlue {
    type Target = WaitableNativeActivityState;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl NativeActivityGlue {
    pub fn new(
        activity: *mut ANativeActivity,
        saved_state: *const libc::c_void,
        saved_state_size: libc::size_t,
    ) -> Self {
        let glue = Self {
            inner: Arc::new(WaitableNativeActivityState::new(
                activity,
                saved_state,
                saved_state_size,
            )),
        };

        let weak_ref = Arc::downgrade(&glue.inner);
        let weak_ptr = Weak::into_raw(weak_ref);
        unsafe {
            (*activity).instance = weak_ptr as *mut _;

            (*(*activity).callbacks).onDestroy = Some(on_destroy);
            (*(*activity).callbacks).onStart = Some(on_start);
            (*(*activity).callbacks).onResume = Some(on_resume);
            (*(*activity).callbacks).onSaveInstanceState = Some(on_save_instance_state);
            (*(*activity).callbacks).onPause = Some(on_pause);
            (*(*activity).callbacks).onStop = Some(on_stop);
            (*(*activity).callbacks).onConfigurationChanged = Some(on_configuration_changed);
            (*(*activity).callbacks).onLowMemory = Some(on_low_memory);
            (*(*activity).callbacks).onWindowFocusChanged = Some(on_window_focus_changed);
            (*(*activity).callbacks).onNativeWindowCreated = Some(on_native_window_created);
            (*(*activity).callbacks).onNativeWindowDestroyed = Some(on_native_window_destroyed);
            (*(*activity).callbacks).onInputQueueCreated = Some(on_input_queue_created);
            (*(*activity).callbacks).onInputQueueDestroyed = Some(on_input_queue_destroyed);
        }

        glue
    }

    /// Returns the file descriptor that needs to be polled by the Rust main thread
    /// for events/commands from the JVM thread
    pub fn cmd_read_fd(&self) -> libc::c_int {
        self.mutex.lock().unwrap().msg_read
    }

    /// For the Rust main thread to read a single pending command sent from the JVM main thread
    pub fn read_cmd(&self) -> Option<AppCmd> {
        self.inner.mutex.lock().unwrap().read_cmd()
    }

    /// For the Rust main thread to get an ndk::InputQueue that wraps the AInputQueue pointer
    /// we have and at the same time ensure that the input queue is attached to the given looper.
    ///
    /// NB: it's expected that the input queue is detached as soon as we know there is new
    /// input (knowing the app will be notified) and only re-attached when the application
    /// reads the input (to avoid lots of redundant wake ups)
    pub fn looper_attached_input_queue(
        &self,
        looper: *mut ndk_sys::ALooper,
        ident: libc::c_int,
    ) -> Option<InputQueue> {
        let mut guard = self.mutex.lock().unwrap();

        if guard.input_queue == ptr::null_mut() {
            return None;
        }

        unsafe {
            // Reattach the input queue to the looper so future input will again deliver an
            // `InputAvailable` event.
            guard.attach_input_queue_to_looper(looper, ident);
            Some(InputQueue::from_ptr(NonNull::new_unchecked(
                guard.input_queue,
            )))
        }
    }

    pub fn detach_input_queue_from_looper(&self) {
        unsafe {
            self.inner
                .mutex
                .lock()
                .unwrap()
                .detach_input_queue_from_looper();
        }
    }

    pub fn config(&self) -> ConfigurationRef {
        self.mutex.lock().unwrap().config.clone()
    }

    pub fn content_rect(&self) -> Rect {
        self.mutex.lock().unwrap().content_rect.into()
    }
}

#[derive(Debug)]
pub struct NativeActivityState {
    pub msg_read: libc::c_int,
    pub msg_write: libc::c_int,
    pub config: super::ConfigurationRef,
    pub saved_state: *mut libc::c_void,
    pub saved_state_size: libc::size_t,
    pub input_queue: *mut ndk_sys::AInputQueue,
    pub window: Option<NativeWindow>,
    pub content_rect: ndk_sys::ARect,
    pub activity_state: State,
    pub destroy_requested: bool,
    pub running: bool,
    pub state_saved: bool,
    pub destroyed: bool,
    pub redraw_needed: bool,
    pub pending_input_queue: *mut ndk_sys::AInputQueue,
    pub pending_window: Option<NativeWindow>,
    pub pending_content_rect: ndk_sys::ARect,
}

impl NativeActivityState {
    pub fn read_cmd(&mut self) -> Option<AppCmd> {
        let mut cmd_i: i8 = 0;
        loop {
            match unsafe { libc::read(self.msg_read, &mut cmd_i as *mut _ as *mut _, 1) } {
                1 => {
                    let cmd = AppCmd::try_from(cmd_i);
                    return match cmd {
                        Ok(AppCmd::SaveState) => {
                            self.free_saved_state();
                            Some(AppCmd::SaveState)
                        }
                        Ok(cmd) => Some(cmd),
                        Err(_) => {
                            log::error!("Spurious, unknown NativeActivityGlue cmd: {}", cmd_i);
                            None
                        }
                    };
                }
                -1 => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        log::error!("Failure reading NativeActivityGlue cmd: {}", err);
                        return None;
                    }
                }
                count => {
                    log::error!(
                        "Spurious read of {count} bytes while reading NativeActivityGlue cmd"
                    );
                    return None;
                }
            }
        }
    }

    fn write_cmd(&mut self, cmd: AppCmd) {
        let cmd = cmd as i8;
        loop {
            match unsafe { libc::write(self.msg_write, &cmd as *const _ as *const _, 1) } {
                1 => break,
                -1 => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        log::error!("Failure writing NativeActivityGlue cmd: {}", err);
                        return;
                    }
                }
                count => {
                    log::error!(
                        "Spurious write of {count} bytes while writing NativeActivityGlue cmd"
                    );
                    return;
                }
            }
        }
    }

    fn free_saved_state(&mut self) {
        if self.saved_state != ptr::null_mut() {
            unsafe { libc::free(self.saved_state) };
            self.saved_state = ptr::null_mut();
            self.saved_state_size = 0;
        }
    }

    pub unsafe fn attach_input_queue_to_looper(
        &mut self,
        looper: *mut ndk_sys::ALooper,
        ident: libc::c_int,
    ) {
        if self.input_queue != ptr::null_mut() {
            log::trace!("Attaching input queue to looper");
            ndk_sys::AInputQueue_attachLooper(
                self.input_queue,
                looper,
                ident,
                None,
                ptr::null_mut(),
            );
        }
    }

    pub unsafe fn detach_input_queue_from_looper(&mut self) {
        if self.input_queue != ptr::null_mut() {
            log::trace!("Detaching input queue from looper");
            ndk_sys::AInputQueue_detachLooper(self.input_queue);
        }
    }
}

impl Drop for WaitableNativeActivityState {
    fn drop(&mut self) {
        log::debug!("WaitableNativeActivityState::drop!");
        unsafe {
            let mut guard = self.mutex.lock().unwrap();
            guard.free_saved_state();
            guard.detach_input_queue_from_looper();
            guard.destroyed = true;
            self.cond.notify_one();
        }
    }
}

impl WaitableNativeActivityState {
    ///////////////////////////////
    // Java-side callback handling
    ///////////////////////////////

    pub fn new(
        activity: *mut ndk_sys::ANativeActivity,
        saved_state_in: *const libc::c_void,
        saved_state_size: libc::size_t,
    ) -> Self {
        let mut msgpipe: [libc::c_int; 2] = [-1, -1];
        unsafe {
            if libc::pipe(msgpipe.as_mut_ptr()) != 0 {
                panic!(
                    "could not create  Rust <-> Java IPC pipe: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        // NB: The implementation for `ANativeActivity` explicitly documents that save_state must
        // be tracked via `malloc()` and `free()`, since `ANativeActivity` may need to free state
        // that it is given via its `onSaveInstanceState` callback.
        let mut saved_state = ptr::null_mut();
        unsafe {
            if saved_state_in != ptr::null() && saved_state_size > 0 {
                saved_state = libc::malloc(saved_state_size);
                assert!(
                    saved_state != ptr::null_mut(),
                    "Failed to allocate {} bytes for restoring saved application state",
                    saved_state_size
                );
                libc::memcpy(saved_state, saved_state_in, saved_state_size);
            }
        }

        let config = unsafe {
            let config = ndk_sys::AConfiguration_new();
            ndk_sys::AConfiguration_fromAssetManager(config, (*activity).assetManager);

            let config = super::ConfigurationRef::new(Configuration::from_ptr(
                NonNull::new_unchecked(config),
            ));
            log::debug!("Config: {:#?}", config);
            config
        };

        Self {
            activity,
            mutex: Mutex::new(NativeActivityState {
                msg_read: msgpipe[0],
                msg_write: msgpipe[1],
                config,
                saved_state,
                saved_state_size,
                input_queue: ptr::null_mut(),
                window: None,
                content_rect: Rect::empty().into(),
                activity_state: State::Init,
                destroy_requested: false,
                running: false,
                state_saved: false,
                destroyed: false,
                redraw_needed: false,
                pending_input_queue: ptr::null_mut(),
                pending_window: None,
                pending_content_rect: Rect::empty().into(),
            }),
            cond: Condvar::new(),
        }
    }

    pub fn notify_destroyed(&self) {
        let mut guard = self.mutex.lock().unwrap();

        unsafe {
            guard.write_cmd(AppCmd::Destroy);
            while !guard.destroyed {
                guard = self.cond.wait(guard).unwrap();
            }

            libc::close(guard.msg_read);
            guard.msg_read = -1;
            libc::close(guard.msg_write);
            guard.msg_write = -1;
        }
    }

    pub fn notify_config_changed(&self) {
        let mut guard = self.mutex.lock().unwrap();
        guard.write_cmd(AppCmd::ConfigChanged);
    }

    pub fn notify_low_memory(&self) {
        let mut guard = self.mutex.lock().unwrap();
        guard.write_cmd(AppCmd::LowMemory);
    }

    pub fn notify_focus_changed(&self, focused: bool) {
        let mut guard = self.mutex.lock().unwrap();
        guard.write_cmd(if focused {
            AppCmd::GainedFocus
        } else {
            AppCmd::LostFocus
        });
    }

    unsafe fn set_input(&self, input_queue: *mut ndk_sys::AInputQueue) {
        let mut guard = self.mutex.lock().unwrap();

        // The pending_input_queue state should only be set while in this method, and since
        // it doesn't allow re-entrance and is cleared before returning then we expect
        // this to be null
        debug_assert!(
            guard.pending_input_queue.is_null(),
            "InputQueue update clash"
        );

        guard.pending_input_queue = input_queue;
        guard.write_cmd(AppCmd::InputQueueChanged);
        while guard.input_queue != guard.pending_input_queue {
            guard = self.cond.wait(guard).unwrap();
        }
        guard.pending_input_queue = ptr::null_mut();
    }

    unsafe fn set_window(&self, window: Option<NativeWindow>) {
        let mut guard = self.mutex.lock().unwrap();

        // The pending_window state should only be set while in this method, and since
        // it doesn't allow re-entrance and is cleared before returning then we expect
        // this to be None
        debug_assert!(guard.pending_window.is_none(), "NativeWindow update clash");

        if guard.window.is_some() {
            guard.write_cmd(AppCmd::TermWindow);
        }
        guard.pending_window = window;
        if guard.pending_window.is_some() {
            guard.write_cmd(AppCmd::InitWindow);
        }
        while guard.window != guard.pending_window {
            guard = self.cond.wait(guard).unwrap();
        }
        guard.pending_window = None;
    }

    unsafe fn set_activity_state(&self, state: State) {
        let mut guard = self.mutex.lock().unwrap();

        let cmd = match state {
            State::Init => panic!("Can't explicitly transition into 'init' state"),
            State::Start => AppCmd::Start,
            State::Resume => AppCmd::Resume,
            State::Pause => AppCmd::Pause,
            State::Stop => AppCmd::Stop,
        };
        guard.write_cmd(cmd);

        while guard.activity_state != state {
            guard = self.cond.wait(guard).unwrap();
        }
    }

    unsafe fn request_save_state(&self) -> (*mut libc::c_void, libc::size_t) {
        let mut guard = self.mutex.lock().unwrap();

        guard.state_saved = false;
        guard.write_cmd(AppCmd::SaveState);
        while guard.state_saved == false {
            guard = self.cond.wait(guard).unwrap();
        }

        let saved_state = std::mem::replace(&mut guard.saved_state, ptr::null_mut());
        let saved_state_size = std::mem::take(&mut guard.saved_state_size);
        if saved_state != ptr::null_mut() && saved_state_size > 0 {
            (saved_state, saved_state_size)
        } else {
            (ptr::null_mut(), 0)
        }
    }

    pub fn saved_state(&self) -> Option<Vec<u8>> {
        let guard = self.mutex.lock().unwrap();

        unsafe {
            if guard.saved_state != ptr::null_mut() && guard.saved_state_size > 0 {
                let buf: &mut [u8] = std::slice::from_raw_parts_mut(
                    guard.saved_state.cast(),
                    guard.saved_state_size as usize,
                );
                let state = buf.to_vec();
                Some(state)
            } else {
                None
            }
        }
    }

    pub fn set_saved_state(&self, state: &[u8]) {
        let mut guard = self.mutex.lock().unwrap();

        // ANativeActivity specifically expects the state to have been allocated
        // via libc::malloc since it will automatically handle freeing the data.

        unsafe {
            // In case the application calls store() multiple times for some reason we
            // make sure to free any pre-existing state...
            if guard.saved_state != ptr::null_mut() {
                libc::free(guard.saved_state);
                guard.saved_state = ptr::null_mut();
                guard.saved_state_size = 0;
            }

            let buf = libc::malloc(state.len());
            if buf == ptr::null_mut() {
                panic!("Failed to allocate save_state buffer");
            }

            // Since it's a byte array there's no special alignment requirement here.
            //
            // Since we re-define `buf` we ensure it's not possible to access the buffer
            // via its original pointer for the lifetime of the slice.
            {
                let buf: &mut [u8] = std::slice::from_raw_parts_mut(buf.cast(), state.len());
                buf.copy_from_slice(state);
            }

            guard.saved_state = buf;
            guard.saved_state_size = state.len() as _;
        }
    }

    ////////////////////////////
    // Rust-side event loop
    ////////////////////////////

    pub fn notify_main_thread_running(&self) {
        let mut guard = self.mutex.lock().unwrap();
        guard.running = true;
        self.cond.notify_one();
    }

    pub unsafe fn pre_exec_cmd(
        &self,
        cmd: AppCmd,
        looper: *mut ndk_sys::ALooper,
        input_queue_ident: libc::c_int,
    ) {
        log::trace!("Pre: AppCmd::{:#?}", cmd);
        match cmd {
            AppCmd::InputQueueChanged => {
                let mut guard = self.mutex.lock().unwrap();
                guard.detach_input_queue_from_looper();
                guard.input_queue = guard.pending_input_queue;
                if guard.input_queue != ptr::null_mut() {
                    guard.attach_input_queue_to_looper(looper, input_queue_ident);
                }
                self.cond.notify_one();
            }
            AppCmd::InitWindow => {
                let mut guard = self.mutex.lock().unwrap();
                guard.window = guard.pending_window.clone();
                self.cond.notify_one();
            }
            AppCmd::Resume | AppCmd::Start | AppCmd::Pause | AppCmd::Stop => {
                let mut guard = self.mutex.lock().unwrap();
                guard.activity_state = match cmd {
                    AppCmd::Start => State::Start,
                    AppCmd::Pause => State::Pause,
                    AppCmd::Resume => State::Resume,
                    AppCmd::Stop => State::Stop,
                    _ => unreachable!(),
                };
                self.cond.notify_one();
            }
            AppCmd::ConfigChanged => {
                let guard = self.mutex.lock().unwrap();
                let config = ndk_sys::AConfiguration_new();
                ndk_sys::AConfiguration_fromAssetManager(config, (*self.activity).assetManager);
                let config = Configuration::from_ptr(NonNull::new_unchecked(config));
                guard.config.replace(config);
                log::debug!("Config: {:#?}", guard.config);
            }
            AppCmd::Destroy => {
                let mut guard = self.mutex.lock().unwrap();
                guard.destroy_requested = true;
            }
            _ => {}
        }
    }

    pub unsafe fn post_exec_cmd(&self, cmd: AppCmd) {
        log::trace!("Post: AppCmd::{:#?}", cmd);
        match cmd {
            AppCmd::TermWindow => {
                let mut guard = self.mutex.lock().unwrap();
                guard.window = None;
                self.cond.notify_one();
            }
            AppCmd::SaveState => {
                let mut guard = self.mutex.lock().unwrap();
                guard.state_saved = true;
                self.cond.notify_one();
            }
            AppCmd::Resume => {
                let mut guard = self.mutex.lock().unwrap();
                guard.free_saved_state();
            }
            _ => {}
        }
    }
}

extern "Rust" {
    pub fn android_main(app: AndroidApp);
}

fn android_log(level: Level, tag: &CStr, msg: &CStr) {
    let prio = match level {
        Level::Error => ndk_sys::android_LogPriority::ANDROID_LOG_ERROR,
        Level::Warn => ndk_sys::android_LogPriority::ANDROID_LOG_WARN,
        Level::Info => ndk_sys::android_LogPriority::ANDROID_LOG_INFO,
        Level::Debug => ndk_sys::android_LogPriority::ANDROID_LOG_DEBUG,
        Level::Trace => ndk_sys::android_LogPriority::ANDROID_LOG_VERBOSE,
    };
    unsafe {
        ndk_sys::__android_log_write(prio.0 as libc::c_int, tag.as_ptr(), msg.as_ptr());
    }
}

unsafe extern "C" fn on_destroy(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("Destroy: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.notify_destroyed()
    }
}

unsafe extern "C" fn on_start(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("Start: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_activity_state(State::Start);
    }
}

unsafe extern "C" fn on_resume(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("Resume: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_activity_state(State::Resume);
    }
}

unsafe extern "C" fn on_save_instance_state(
    activity: *mut ndk_sys::ANativeActivity,
    out_len: *mut ndk_sys::size_t,
) -> *mut libc::c_void {
    log::debug!("SaveInstanceState: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        let (state, len) = waitable_activity.request_save_state();
        *out_len = len as ndk_sys::size_t;
        state
    } else {
        *out_len = 0;
        ptr::null_mut()
    }
}

unsafe extern "C" fn on_pause(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("Pause: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_activity_state(State::Pause);
    }
}

unsafe extern "C" fn on_stop(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("Stop: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_activity_state(State::Stop);
    }
}

unsafe extern "C" fn on_configuration_changed(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("ConfigurationChanged: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.notify_config_changed();
    }
}

unsafe extern "C" fn on_low_memory(activity: *mut ndk_sys::ANativeActivity) {
    log::debug!("LowMemory: {:p}\n", activity);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.notify_low_memory();
    }
}

unsafe extern "C" fn on_window_focus_changed(
    activity: *mut ndk_sys::ANativeActivity,
    focused: libc::c_int,
) {
    log::debug!("WindowFocusChanged: {:p} -- {}\n", activity, focused);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.notify_focus_changed(focused != 0);
    }
}

unsafe extern "C" fn on_native_window_created(
    activity: *mut ndk_sys::ANativeActivity,
    window: *mut ndk_sys::ANativeWindow,
) {
    log::debug!("NativeWindowCreated: {:p} -- {:p}\n", activity, window);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        // It's important that we use ::clone_from_ptr() here because NativeWindow
        // has a Drop implementation that will unconditionally _release() the native window
        let window = NativeWindow::clone_from_ptr(NonNull::new_unchecked(window));
        waitable_activity.set_window(Some(window));
    }
}

unsafe extern "C" fn on_native_window_destroyed(
    activity: *mut ndk_sys::ANativeActivity,
    window: *mut ndk_sys::ANativeWindow,
) {
    log::debug!("NativeWindowDestroyed: {:p} -- {:p}\n", activity, window);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_window(None);
    }
}

unsafe extern "C" fn on_input_queue_created(
    activity: *mut ndk_sys::ANativeActivity,
    queue: *mut ndk_sys::AInputQueue,
) {
    log::debug!("InputQueueCreated: {:p} -- {:p}\n", activity, queue);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_input(queue);
    }
}

unsafe extern "C" fn on_input_queue_destroyed(
    activity: *mut ndk_sys::ANativeActivity,
    queue: *mut ndk_sys::AInputQueue,
) {
    log::debug!("InputQueueDestroyed: {:p} -- {:p}\n", activity, queue);
    let weak_ptr: *const WaitableNativeActivityState = (*activity).instance.cast();
    if let Some(waitable_activity) = Weak::from_raw(weak_ptr).upgrade() {
        waitable_activity.set_input(ptr::null_mut());
    }
}

/// This is the native entrypoint for our cdylib library that `ANativeActivity` will look for via `dlsym`
#[no_mangle]
extern "C" fn ANativeActivity_onCreate(
    activity: *mut ndk_sys::ANativeActivity,
    saved_state: *const libc::c_void,
    saved_state_size: libc::size_t,
) {
    log::debug!("Creating: {:p}", activity);

    // Maybe make this stdout/stderr redirection an optional / opt-in feature?...
    unsafe {
        let mut logpipe: [RawFd; 2] = Default::default();
        libc::pipe(logpipe.as_mut_ptr());
        libc::dup2(logpipe[1], libc::STDOUT_FILENO);
        libc::dup2(logpipe[1], libc::STDERR_FILENO);
        std::thread::spawn(move || {
            let tag = CStr::from_bytes_with_nul(b"RustStdoutStderr\0").unwrap();
            let file = File::from_raw_fd(logpipe[0]);
            let mut reader = BufReader::new(file);
            let mut buffer = String::new();
            loop {
                buffer.clear();
                if let Ok(len) = reader.read_line(&mut buffer) {
                    if len == 0 {
                        break;
                    } else if let Ok(msg) = CString::new(buffer.clone()) {
                        android_log(Level::Info, tag, &msg);
                    }
                }
            }
        });
    }

    // Conceptually we associate a glue reference with the JVM main thread, and another
    // reference with the Rust main thread
    let jvm_glue = NativeActivityGlue::new(activity, saved_state, saved_state_size);

    let rust_glue = jvm_glue.clone();
    // Let us Send the NativeActivity pointer to the Rust main() thread without a wrapper type
    let activity_ptr: libc::intptr_t = activity as _;

    // Note: we drop the thread handle which will detach the thread
    std::thread::spawn(move || {
        let activity: *mut ANativeActivity = activity_ptr as *mut _;

        let jvm = unsafe {
            let na = activity;
            let jvm = (*na).vm;
            let activity = (*na).clazz; // Completely bogus name; this is the _instance_ not class pointer
            ndk_context::initialize_android_context(jvm.cast(), activity.cast());

            // Since this is a newly spawned thread then the JVM hasn't been attached
            // to the thread yet. Attach before calling the applications main function
            // so they can safely make JNI calls
            let mut jenv_out: *mut core::ffi::c_void = std::ptr::null_mut();
            if let Some(attach_current_thread) = (*(*jvm)).AttachCurrentThread {
                attach_current_thread(jvm, &mut jenv_out, std::ptr::null_mut());
            }

            jvm
        };

        let app = AndroidApp::new(rust_glue.clone());

        rust_glue.notify_main_thread_running();

        unsafe {
            // XXX: If we were in control of the Java Activity subclass then
            // we could potentially run the android_main function via a Java native method
            // springboard (e.g. call an Activity subclass method that calls a jni native
            // method that then just calls android_main()) that would make sure there was
            // a Java frame at the base of our call stack which would then be recognised
            // when calling FindClass to lookup a suitable classLoader, instead of
            // defaulting to the system loader. Without this then it's difficult for native
            // code to look up non-standard Java classes.
            android_main(app);

            // Since this is a newly spawned thread then the JVM hasn't been attached
            // to the thread yet. Attach before calling the applications main function
            // so they can safely make JNI calls
            if let Some(detach_current_thread) = (*(*jvm)).DetachCurrentThread {
                detach_current_thread(jvm);
            }

            ndk_context::release_android_context();
        }
    });

    // Wait for thread to start.
    let mut guard = jvm_glue.mutex.lock().unwrap();
    while !guard.running {
        guard = jvm_glue.cond.wait(guard).unwrap();
    }
}
