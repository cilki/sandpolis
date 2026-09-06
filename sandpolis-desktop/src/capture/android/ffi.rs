//! JNI bridge to the Android main service that produces screen frames and
//! accepts injected input events.

use jni::Env;
use jni::EnvUnowned;
use jni::errors::LogErrorAndDefault;
use jni::errors::{Error as JniError, Result as JniResult};
use jni::objects::JByteBuffer;
use jni::objects::JString;
use jni::objects::JValue;
use jni::objects::{JClass, JObject};
use jni::refs::Global;
use jni::sys::jboolean;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str};

use lazy_static::lazy_static;
use std::ops::Not;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicPtr, Ordering::SeqCst};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

/// A global reference to a plain Java object, which is what both contexts below
/// are held as.
type GlobalObject = Global<JObject<'static>>;

lazy_static! {
    static ref JVM: RwLock<Option<JavaVM>> = RwLock::new(None);
    static ref MAIN_SERVICE_CTX: RwLock<Option<GlobalObject>> = RwLock::new(None); // MainService -> video service / info
    static ref APPLICATION_CONTEXT: RwLock<Option<GlobalObject>> = RwLock::new(None);
    static ref VIDEO_RAW: Mutex<FrameRaw> = Mutex::new(FrameRaw::new("video", MAX_VIDEO_FRAME_TIMEOUT));
    static ref NDK_CONTEXT_LOCK: Mutex<()> = Default::default();
}

const MAX_VIDEO_FRAME_TIMEOUT: Duration = Duration::from_millis(100);

struct FrameRaw {
    name: &'static str,
    ptr: AtomicPtr<u8>,
    len: usize,
    last_update: Instant,
    timeout: Duration,
    enable: bool,
}

impl FrameRaw {
    fn new(name: &'static str, timeout: Duration) -> Self {
        FrameRaw {
            name,
            ptr: AtomicPtr::default(),
            len: 0,
            last_update: Instant::now(),
            timeout,
            enable: false,
        }
    }

    fn set_enable(&mut self, value: bool) {
        self.enable = value;
        self.ptr.store(std::ptr::null_mut(), SeqCst);
        self.len = 0;
    }

    fn update(&mut self, data: *mut u8, len: usize) {
        if self.enable.not() {
            return;
        }
        self.len = len;
        self.ptr.store(data, SeqCst);
        self.last_update = Instant::now();
    }

    // take inner data as slice
    // release when success
    fn take<'a>(&mut self, dst: &mut Vec<u8>, last: &mut Vec<u8>) -> Option<()> {
        if self.enable.not() {
            return None;
        }
        let ptr = self.ptr.load(SeqCst);
        if ptr.is_null() || self.len == 0 {
            None
        } else {
            if self.last_update.elapsed() > self.timeout {
                tracing::trace!("Failed to take {} raw,timeout!", self.name);
                return None;
            }
            let slice = unsafe { std::slice::from_raw_parts(ptr, self.len) };
            self.release();
            if last.len() == slice.len()
                && crate::capture::would_block_if_equal(last, slice).is_err()
            {
                return None;
            }
            dst.resize(slice.len(), 0);
            unsafe {
                std::ptr::copy_nonoverlapping(slice.as_ptr(), dst.as_mut_ptr(), slice.len());
            }
            Some(())
        }
    }

    fn release(&mut self) {
        self.len = 0;
        self.ptr.store(std::ptr::null_mut(), SeqCst);
    }
}

pub fn get_video_raw<'a>(dst: &mut Vec<u8>, last: &mut Vec<u8>) -> Option<()> {
    VIDEO_RAW.lock().ok()?.take(dst, last)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ffi_FFI_onVideoFrameUpdate<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    buffer: JByteBuffer<'local>,
) {
    env.with_env(|env| -> JniResult<()> {
        let data = env.get_direct_buffer_address(&buffer)?;
        let len = env.get_direct_buffer_capacity(&buffer)?;
        VIDEO_RAW.lock().unwrap().update(data, len);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ffi_FFI_setFrameRawEnable<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
    value: jboolean,
) {
    env.with_env(|env| -> JniResult<()> {
        if name.try_to_string(env)? == "video" {
            VIDEO_RAW.lock().unwrap().set_enable(value);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ffi_FFI_init<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ctx: JObject<'local>,
) {
    tracing::debug!("MainService init from java");
    env.with_env(|env| -> JniResult<()> {
        let jvm = env.get_java_vm()?;
        let java_vm = jvm.get_raw() as *mut c_void;

        let mut jvm_lock = JVM.write().unwrap();
        if jvm_lock.is_none() {
            *jvm_lock = Some(jvm);
        }
        drop(jvm_lock);

        let context = env.new_global_ref(&ctx)?;
        let context_jobject = context.as_raw() as *mut c_void;
        *MAIN_SERVICE_CTX.write().unwrap() = Some(context);
        init_ndk_context(java_vm, context_jobject);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Attach the calling thread and run `f` against the `MainService` context, if
/// the service has called into [`Java_ffi_FFI_init`] already.
fn with_main_service<T>(f: impl FnOnce(&mut Env, &JObject<'static>) -> JniResult<T>) -> JniResult<T> {
    let jvm = JVM.read().unwrap();
    let ctx = MAIN_SERVICE_CTX.read().unwrap();

    let (Some(jvm), Some(ctx)) = (jvm.as_ref(), ctx.as_ref()) else {
        return Err(JniError::NullPtr("MainService context is not initialized"));
    };

    jvm.attach_current_thread(|env| f(env, ctx))
}

pub fn call_main_service_pointer_input(kind: &str, mask: i32, x: i32, y: i32) -> JniResult<()> {
    let kind = if kind == "touch" { 0 } else { 1 };
    with_main_service(|env, ctx| {
        env.call_method(
            ctx,
            jni_str!("rustPointerInput"),
            jni_sig!((int, int, int, int) -> void),
            &[
                JValue::Int(kind),
                JValue::Int(mask),
                JValue::Int(x),
                JValue::Int(y),
            ],
        )?;
        Ok(())
    })
}

pub fn call_main_service_key_event(data: &[u8]) -> JniResult<()> {
    with_main_service(|env, ctx| {
        let data = env.byte_array_from_slice(data)?;
        env.call_method(
            ctx,
            jni_str!("rustKeyEventInput"),
            jni_sig!((byte[]) -> void),
            &[JValue::Object(&data)],
        )?;
        Ok(())
    })
}

pub fn call_main_service_get_by_name(name: &str) -> JniResult<String> {
    with_main_service(|env, ctx| {
        let name = env.new_string(name)?;
        let res = env
            .call_method(
                ctx,
                jni_str!("rustGetByName"),
                jni_sig!((java.lang.String) -> java.lang.String),
                &[JValue::Object(&name)],
            )?
            .l()?;
        let res = env.cast_local::<JString>(res)?;
        res.try_to_string(env)
    })
}

pub fn call_main_service_set_by_name(
    name: &str,
    arg1: Option<&str>,
    arg2: Option<&str>,
) -> JniResult<()> {
    with_main_service(|env, ctx| {
        let name = env.new_string(name)?;
        let arg1 = env.new_string(arg1.unwrap_or(""))?;
        let arg2 = env.new_string(arg2.unwrap_or(""))?;

        env.call_method(
            ctx,
            jni_str!("rustSetByName"),
            jni_sig!((java.lang.String, java.lang.String, java.lang.String) -> void),
            &[
                JValue::Object(&name),
                JValue::Object(&arg1),
                JValue::Object(&arg2),
            ],
        )?;
        Ok(())
    })
}

// android-activity's glue entry registers the activity context in ndk-context
// at startup and asserts nothing registered before it — rustdesk's JNI_OnLoad
// hook (which registered here at System.loadLibrary time, with a null context)
// aborted every launch of the mobile app. So the glue owns the initial
// registration, and the service can only ever *replace* it.
fn init_ndk_context(java_vm: *mut c_void, context_jobject: *mut c_void) {
    let _lock = NDK_CONTEXT_LOCK.lock().unwrap();
    unsafe {
        ndk_context::release_android_context();
        ndk_context::initialize_android_context(java_vm, context_jobject);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ffi_FFI_onAppStart<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ctx: JObject<'local>,
) {
    env.with_env(|env| -> JniResult<()> {
        if ctx.is_null() {
            tracing::error!("application context is null");
            return Ok(());
        }
        if APPLICATION_CONTEXT.read().unwrap().is_some() {
            tracing::info!("application context already initialized");
            return Ok(());
        }
        let context = env.new_global_ref(&ctx)?;
        *APPLICATION_CONTEXT.write().unwrap() = Some(context);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}
