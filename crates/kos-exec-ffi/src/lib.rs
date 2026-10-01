// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use kos_exec::context::AppContext;
use kos_exec::error::KosError;
use kos_exec::traits::{ErrorAction, KosApp};
use kos_exec::{ShmPublisher, ShmSubscriber, ShmTransport, TopicPublisher, TopicSubscriber};

const KOS_OK: c_int = 0;
const KOS_ERROR: c_int = -1;

const KOS_ACTION_RESTART: c_int = 0;
const KOS_ACTION_TERMINATE: c_int = 1;
const KOS_ACTION_IGNORE: c_int = 2;

type KosInitFn = Option<unsafe extern "C" fn(*mut c_void, *mut KosContext) -> c_int>;
type KosRunFn = Option<unsafe extern "C" fn(*mut c_void, *mut KosContext) -> c_int>;
type KosSuspendFn = Option<unsafe extern "C" fn(*mut c_void, *mut KosContext) -> c_int>;
type KosResumeFn = Option<unsafe extern "C" fn(*mut c_void, *mut KosContext) -> c_int>;
type KosTerminateFn = Option<unsafe extern "C" fn(*mut c_void, *mut KosContext) -> c_int>;
type KosErrorFn =
    Option<unsafe extern "C" fn(*mut c_void, *mut KosContext, *const KosErrorC) -> c_int>;

#[repr(C)]
pub struct KosAppCallbacks {
    pub on_init: KosInitFn,
    pub on_run: KosRunFn,
    pub on_suspend: KosSuspendFn,
    pub on_resume: KosResumeFn,
    pub on_terminate: KosTerminateFn,
    pub on_error: KosErrorFn,
}

#[repr(C)]
pub struct KosErrorC {
    pub code: c_int,
    pub message: *const c_char,
}

pub struct KosContext {
    inner: AppContext,
    string_cache: Vec<CString>,
    transport: Option<ShmTransport>,
}

struct CApp {
    data: *mut c_void,
    callbacks: KosAppCallbacks,
}

unsafe impl Send for CApp {}

impl KosApp for CApp {
    fn on_init(&mut self, ctx: &mut AppContext) -> kos_exec::Result<()> {
        call_c_callback(self.callbacks.on_init, self.data, ctx)
    }

    fn on_run(&mut self, ctx: &mut AppContext) -> kos_exec::Result<()> {
        call_c_callback(self.callbacks.on_run, self.data, ctx)
    }

    fn on_suspend(&mut self, ctx: &mut AppContext) -> kos_exec::Result<()> {
        call_c_callback(self.callbacks.on_suspend, self.data, ctx)
    }

    fn on_resume(&mut self, ctx: &mut AppContext) -> kos_exec::Result<()> {
        call_c_callback(self.callbacks.on_resume, self.data, ctx)
    }

    fn on_terminate(&mut self, ctx: &mut AppContext) -> kos_exec::Result<()> {
        call_c_callback(self.callbacks.on_terminate, self.data, ctx)
    }

    fn on_error(&mut self, ctx: &mut AppContext, error: &KosError) -> ErrorAction {
        let Some(cb) = self.callbacks.on_error else {
            return ErrorAction::Restart;
        };
        let msg = CString::new(format!("{error}")).unwrap_or_default();
        let c_err = KosErrorC {
            code: KOS_ERROR,
            message: msg.as_ptr(),
        };
        let mut wrapper = KosContext {
            inner: AppContext::new(ctx.app_id(), ctx.domain(), ctx.params().clone()),
            string_cache: Vec::new(),
            transport: Some(ShmTransport::new(ctx.app_id())),
        };
        let action = unsafe { cb(self.data, &mut wrapper, &c_err) };
        match action {
            KOS_ACTION_RESTART => ErrorAction::Restart,
            KOS_ACTION_TERMINATE => ErrorAction::Terminate,
            KOS_ACTION_IGNORE => ErrorAction::Ignore,
            _ => ErrorAction::Restart,
        }
    }
}

fn call_c_callback(
    cb: Option<unsafe extern "C" fn(*mut c_void, *mut KosContext) -> c_int>,
    data: *mut c_void,
    ctx: &mut AppContext,
) -> kos_exec::Result<()> {
    let Some(f) = cb else {
        return Ok(());
    };
    let mut wrapper = KosContext {
        inner: AppContext::new(ctx.app_id(), ctx.domain(), ctx.params().clone()),
        string_cache: Vec::new(),
        transport: Some(ShmTransport::new(ctx.app_id())),
    };
    let rc = unsafe { f(data, &mut wrapper) };
    if rc == KOS_OK {
        Ok(())
    } else {
        Err(KosError::InvalidConfig("C callback returned KOS_ERROR".into()))
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_app_run(
    app_id: *const c_char,
    data: *mut c_void,
    _data_size: usize,
    callbacks: *const KosAppCallbacks,
) {
    if app_id.is_null() || callbacks.is_null() {
        return;
    }
    let app_id_str = CStr::from_ptr(app_id).to_string_lossy().into_owned();
    let cbs = ptr::read(callbacks);

    let app = CApp {
        data,
        callbacks: cbs,
    };

    let mut config = kos_exec::RuntimeConfig::from_env(&app_id_str);
    config.app_id = app_id_str;

    let _ = kos_exec::runtime::run(app, config);
}

#[no_mangle]
pub unsafe extern "C" fn kos_app_id(ctx: *mut KosContext) -> *const c_char {
    if ctx.is_null() {
        return ptr::null();
    }
    let ctx = &mut *ctx;
    let s = CString::new(ctx.inner.app_id()).unwrap_or_default();
    let ptr = s.as_ptr();
    ctx.string_cache.push(s);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn kos_domain(ctx: *mut KosContext) -> *const c_char {
    if ctx.is_null() {
        return ptr::null();
    }
    let ctx = &mut *ctx;
    let s = CString::new(ctx.inner.domain()).unwrap_or_default();
    let ptr = s.as_ptr();
    ctx.string_cache.push(s);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn kos_param_float(
    ctx: *mut KosContext,
    key: *const c_char,
    default_val: c_double,
) -> c_double {
    if ctx.is_null() || key.is_null() {
        return default_val;
    }
    let ctx = &mut *ctx;
    let key_str = CStr::from_ptr(key).to_string_lossy();
    ctx.inner.param::<f64>(&key_str).unwrap_or(default_val)
}

#[no_mangle]
pub unsafe extern "C" fn kos_param_int(
    ctx: *mut KosContext,
    key: *const c_char,
    default_val: i64,
) -> i64 {
    if ctx.is_null() || key.is_null() {
        return default_val;
    }
    let ctx = &mut *ctx;
    let key_str = CStr::from_ptr(key).to_string_lossy();
    ctx.inner.param::<i64>(&key_str).unwrap_or(default_val)
}

#[no_mangle]
pub unsafe extern "C" fn kos_param_str(
    ctx: *mut KosContext,
    key: *const c_char,
    default_val: *const c_char,
) -> *const c_char {
    if ctx.is_null() || key.is_null() {
        return default_val;
    }
    let ctx = &mut *ctx;
    let key_str = CStr::from_ptr(key).to_string_lossy();
    match ctx.inner.param::<String>(&key_str) {
        Some(val) => {
            let s = CString::new(val).unwrap_or_default();
            let ptr = s.as_ptr();
            ctx.string_cache.push(s);
            ptr
        }
        None => default_val,
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_log_info(ctx: *mut KosContext, msg: *const c_char) {
    if ctx.is_null() || msg.is_null() {
        return;
    }
    let ctx = &mut *ctx;
    let msg_str = CStr::from_ptr(msg).to_string_lossy();
    ctx.inner.log_info(&msg_str);
}

#[no_mangle]
pub unsafe extern "C" fn kos_log_warn(ctx: *mut KosContext, msg: *const c_char) {
    if ctx.is_null() || msg.is_null() {
        return;
    }
    let ctx = &mut *ctx;
    let msg_str = CStr::from_ptr(msg).to_string_lossy();
    ctx.inner.log_warn(&msg_str);
}

#[no_mangle]
pub unsafe extern "C" fn kos_log_error(ctx: *mut KosContext, msg: *const c_char) {
    if ctx.is_null() || msg.is_null() {
        return;
    }
    let ctx = &mut *ctx;
    let msg_str = CStr::from_ptr(msg).to_string_lossy();
    ctx.inner.log_error(&msg_str);
}

#[no_mangle]
pub unsafe extern "C" fn kos_advertise(
    ctx: *mut KosContext,
    topic: *const c_char,
    msg_size: usize,
) -> *mut c_void {
    if ctx.is_null() || topic.is_null() || msg_size == 0 || msg_size > kos_exec::SHM_MSG_SIZE {
        return ptr::null_mut();
    }
    let ctx = &mut *ctx;
    let topic_str = CStr::from_ptr(topic).to_string_lossy();

    let transport = match ctx.transport.as_mut() {
        Some(t) => t,
        None => return ptr::null_mut(),
    };

    match transport.publisher(&topic_str) {
        Ok(pub_) => Box::into_raw(Box::new(FfiPublisher { inner: pub_, msg_size })) as *mut c_void,
        Err(_) => ptr::null_mut(),
    }
}

struct FfiPublisher {
    inner: ShmPublisher,
    msg_size: usize,
}

#[no_mangle]
pub unsafe extern "C" fn kos_subscribe(
    ctx: *mut KosContext,
    topic: *const c_char,
    _depth: usize,
    _msg_size: usize,
) -> *mut c_void {
    if ctx.is_null() || topic.is_null() {
        return ptr::null_mut();
    }
    let ctx = &mut *ctx;
    let topic_str = CStr::from_ptr(topic).to_string_lossy();

    let transport = match ctx.transport.as_mut() {
        Some(t) => t,
        None => return ptr::null_mut(),
    };

    match transport.subscriber(&topic_str) {
        Ok(sub) => Box::into_raw(Box::new(sub)) as *mut c_void,
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_publish(
    pub_handle: *mut c_void,
    data: *const c_void,
) -> c_int {
    if pub_handle.is_null() || data.is_null() {
        return KOS_ERROR;
    }
    let publisher = &mut *(pub_handle as *mut FfiPublisher);
    let slice = std::slice::from_raw_parts(data as *const u8, publisher.msg_size);
    match publisher.inner.publish(slice) {
        Ok(()) => KOS_OK,
        Err(_) => KOS_ERROR,
    }
}

#[repr(C)]
pub struct KosRecvResult {
    pub data: *const u8,
    pub len: usize,
    capacity: usize,
}

#[no_mangle]
pub unsafe extern "C" fn kos_recv(sub_handle: *mut c_void) -> *mut KosRecvResult {
    if sub_handle.is_null() {
        return ptr::null_mut();
    }
    let subscriber = &mut *(sub_handle as *mut ShmSubscriber);
    match subscriber.recv_copy() {
        Ok(data) => {
            let len = data.len();
            let capacity = data.capacity();
            let data_ptr = data.as_ptr();
            std::mem::forget(data);
            Box::into_raw(Box::new(KosRecvResult {
                data: data_ptr,
                len,
                capacity,
            }))
        }
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_recv_free(result: *mut KosRecvResult) {
    if !result.is_null() {
        let r = Box::from_raw(result);
        drop(Vec::from_raw_parts(r.data as *mut u8, r.len, r.capacity));
    }
}

const KOS_ERR_TIMEOUT: c_int = -2;
const KOS_ERR_NOT_FOUND: c_int = -3;
const KOS_ERR_TRUNCATED: c_int = -4;
const KOS_ERR_REMOTE: c_int = -5;

const FFI_SERVICE_REPLY_MAX: usize = 64 * 1024;

pub type KosServiceFn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        req: *const c_void,
        req_size: usize,
        resp: *mut c_void,
        resp_capacity: usize,
        resp_size: *mut usize,
    ) -> c_int,
>;

struct UserData(*mut c_void);
unsafe impl Send for UserData {}
unsafe impl Sync for UserData {}

impl UserData {
    fn get(&self) -> *mut c_void {
        self.0
    }
}

pub struct KosService {
    _server: kos_exec::service::ServiceServer,
}

#[no_mangle]
pub unsafe extern "C" fn kos_advertise_service(
    service: *const c_char,
    handler: KosServiceFn,
    user_data: *mut c_void,
) -> *mut KosService {
    if service.is_null() {
        return ptr::null_mut();
    }
    let Some(handler) = handler else {
        return ptr::null_mut();
    };
    let name = CStr::from_ptr(service).to_string_lossy().into_owned();
    let user_data = UserData(user_data);
    let result = kos_exec::service::ServiceServer::advertise(&name, move |req| {
        let mut resp = vec![0u8; FFI_SERVICE_REPLY_MAX];
        let mut resp_size = 0usize;
        let rc = handler(
            user_data.get(),
            req.as_ptr().cast(),
            req.len(),
            resp.as_mut_ptr().cast(),
            resp.len(),
            &mut resp_size,
        );
        if rc != KOS_OK {
            return Err(format!("handler returned {rc}"));
        }
        resp.truncate(resp_size.min(FFI_SERVICE_REPLY_MAX));
        Ok(resp)
    });
    match result {
        Ok(server) => Box::into_raw(Box::new(KosService { _server: server })),
        Err(e) => {
            eprintln!("[kos-exec] kos_advertise_service('{name}'): {e}");
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_service_destroy(svc: *mut KosService) {
    if !svc.is_null() {
        drop(Box::from_raw(svc));
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_call_service(
    _ctx: *mut KosContext,
    service: *const c_char,
    req: *const c_void,
    req_size: usize,
    resp: *mut c_void,
    resp_size: usize,
    resp_len: *mut usize,
    timeout_ms: u32,
) -> c_int {
    if service.is_null() || (req.is_null() && req_size > 0) || (resp.is_null() && resp_size > 0) {
        return KOS_ERROR;
    }
    let name = CStr::from_ptr(service).to_string_lossy();
    let request: &[u8] = if req_size == 0 { &[] } else { std::slice::from_raw_parts(req.cast(), req_size) };
    let timeout = if timeout_ms == 0 {
        kos_exec::service::DEFAULT_CALL_TIMEOUT
    } else {
        std::time::Duration::from_millis(timeout_ms as u64)
    };
    match kos_exec::service::call(&name, request, timeout) {
        Ok(reply) => {
            if !resp_len.is_null() {
                *resp_len = reply.len();
            }
            if reply.len() > resp_size {
                return KOS_ERR_TRUNCATED;
            }
            if !reply.is_empty() {
                ptr::copy_nonoverlapping(reply.as_ptr(), resp.cast::<u8>(), reply.len());
            }
            KOS_OK
        }
        Err(KosError::Timeout(_)) => KOS_ERR_TIMEOUT,
        Err(KosError::NotFound(_)) => KOS_ERR_NOT_FOUND,
        Err(KosError::Remote(_)) => KOS_ERR_REMOTE,
        Err(_) => KOS_ERROR,
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_publisher_destroy(pub_handle: *mut c_void) {
    if !pub_handle.is_null() {
        drop(Box::from_raw(pub_handle as *mut FfiPublisher));
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_subscriber_destroy(sub_handle: *mut c_void) {
    if !sub_handle.is_null() {
        drop(Box::from_raw(sub_handle as *mut ShmSubscriber));
    }
}

const KOS_PERIODIC: c_int = 0;
#[allow(dead_code)]
const KOS_EVENT: c_int = 1;

const KOS_CRITICAL: c_int = 0;
const KOS_HIGH: c_int = 1;
const KOS_NORMAL: c_int = 2;
const KOS_LOW: c_int = 3;

#[repr(C)]
pub struct KosThreadConfig {
    pub trigger_type: c_int,
    pub period_ms: u32,
    pub event_topic: *const c_char,
    pub priority: c_int,
    pub cpu_affinity: c_int,
    pub subs: *const *const c_char,
    pub subs_count: u32,
    pub pubs: *const *const c_char,
    pub pubs_count: u32,
}

#[repr(C)]
pub struct KosThreadCallbacksC {
    pub on_init: Option<unsafe extern "C" fn(*mut KosThreadContext, *mut c_void) -> c_int>,
    pub on_run: Option<unsafe extern "C" fn(*mut KosThreadContext, *mut c_void)>,
    pub on_shutdown: Option<unsafe extern "C" fn(*mut KosThreadContext, *mut c_void)>,
    pub user_data: *mut c_void,
}

pub struct KosNode {
    node: kos_exec::Node,
    stop: AtomicBool,
}

pub struct KosThreadContext {
    _private: [u8; 0],
}

unsafe fn thread_ctx<'a>(ctx: *mut KosThreadContext) -> &'a kos_exec::ThreadContext {
    &*(ctx as *const kos_exec::ThreadContext)
}

fn as_c_ctx(ctx: &kos_exec::ThreadContext) -> *mut KosThreadContext {
    ctx as *const kos_exec::ThreadContext as *mut KosThreadContext
}

#[derive(Clone, Copy)]
struct UserDataPtr(*mut c_void);
unsafe impl Send for UserDataPtr {}
unsafe impl Sync for UserDataPtr {}

unsafe fn c_string_array_to_vec(arr: *const *const c_char, count: u32) -> Vec<String> {
    if arr.is_null() || count == 0 {
        return Vec::new();
    }
    (0..count as usize)
        .filter_map(|i| {
            let ptr = *arr.add(i);
            if ptr.is_null() {
                None
            } else {
                Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
            }
        })
        .collect()
}

fn c_priority_to_priority(p: c_int) -> kos_exec::Priority {
    match p {
        KOS_CRITICAL => kos_exec::Priority::Critical,
        KOS_HIGH => kos_exec::Priority::High,
        KOS_NORMAL => kos_exec::Priority::Normal,
        KOS_LOW => kos_exec::Priority::Low,
        _ => kos_exec::Priority::Normal,
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_node_new(name: *const c_char) -> *mut KosNode {
    if name.is_null() {
        return ptr::null_mut();
    }
    let name_str = CStr::from_ptr(name).to_string_lossy();
    let config = kos_exec::NodeConfig::periodic(std::time::Duration::from_millis(10));
    let mut node = kos_exec::Node::new(&name_str, config);
    node.set_transport(Arc::new(ShmTransport::new(&name_str)));
    Box::into_raw(Box::new(KosNode { node, stop: AtomicBool::new(false) }))
}

#[no_mangle]
pub unsafe extern "C" fn kos_node_destroy(node: *mut KosNode) {
    if !node.is_null() {
        let mut node = Box::from_raw(node);
        node.node.shutdown();
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_create_thread(
    node: *mut KosNode,
    name: *const c_char,
    config: *const KosThreadConfig,
    callbacks: *const KosThreadCallbacksC,
) -> c_int {
    if node.is_null() || name.is_null() || config.is_null() || callbacks.is_null() {
        return KOS_ERROR;
    }

    let node = &mut *node;
    let name_str = CStr::from_ptr(name).to_string_lossy().into_owned();
    let cfg = &*config;
    let cbs = &*callbacks;

    let trigger = if cfg.trigger_type == KOS_PERIODIC {
        kos_exec::Trigger::Periodic(std::time::Duration::from_millis(cfg.period_ms as u64))
    } else {
        let event = if cfg.event_topic.is_null() {
            String::new()
        } else {
            CStr::from_ptr(cfg.event_topic).to_string_lossy().into_owned()
        };
        kos_exec::Trigger::Event(event)
    };

    let subs = c_string_array_to_vec(cfg.subs, cfg.subs_count);
    let pubs = c_string_array_to_vec(cfg.pubs, cfg.pubs_count);

    let thread_config = kos_exec::ThreadConfig {
        name: name_str,
        trigger,
        priority: c_priority_to_priority(cfg.priority),
        cpu_affinity: if cfg.cpu_affinity >= 0 { Some(cfg.cpu_affinity as usize) } else { None },
        subs,
        pubs,
        threshold: None,
    };

    let ud = UserDataPtr(cbs.user_data);
    let c_on_run = cbs.on_run;
    let c_on_init = cbs.on_init;
    let c_on_shutdown = cbs.on_shutdown;

    let on_run_addr: usize = c_on_run.map_or(0, |f| f as usize);
    let ud_addr: usize = ud.0 as usize;

    let mut thread_callbacks = kos_exec::ThreadCallbacks::new(move |ctx| {
        if on_run_addr != 0 {
            let f: unsafe extern "C" fn(*mut KosThreadContext, *mut c_void) =
                unsafe { std::mem::transmute(on_run_addr) };
            unsafe { f(as_c_ctx(ctx), ud_addr as *mut c_void) };
        }
    });

    if let Some(init_fn) = c_on_init {
        let init_addr: usize = init_fn as usize;
        thread_callbacks = thread_callbacks.with_init(move |ctx| {
            let f: unsafe extern "C" fn(*mut KosThreadContext, *mut c_void) -> c_int =
                unsafe { std::mem::transmute(init_addr) };
            let rc = unsafe { f(as_c_ctx(ctx), ud_addr as *mut c_void) };
            if rc == KOS_OK {
                Ok(())
            } else {
                Err(kos_exec::KosError::InvalidConfig("C on_init failed".into()))
            }
        });
    }

    if let Some(shutdown_fn) = c_on_shutdown {
        let shutdown_addr: usize = shutdown_fn as usize;
        thread_callbacks = thread_callbacks.with_shutdown(move |ctx| {
            let f: unsafe extern "C" fn(*mut KosThreadContext, *mut c_void) =
                unsafe { std::mem::transmute(shutdown_addr) };
            unsafe { f(as_c_ctx(ctx), ud_addr as *mut c_void) };
        });
    }

    match node.node.create_thread(thread_config, thread_callbacks) {
        Ok(()) => KOS_OK,
        Err(_) => KOS_ERROR,
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_node_spin(node: *mut KosNode) -> c_int {
    if node.is_null() {
        return KOS_ERROR;
    }
    let node = &mut *node;
    if node.node.spin().is_err() {
        return KOS_ERROR;
    }
    while !node.stop.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    node.node.shutdown();
    KOS_OK
}

#[no_mangle]
pub unsafe extern "C" fn kos_node_shutdown(node: *mut KosNode) {
    if !node.is_null() {
        (*node).stop.store(true, Ordering::Release);
    }
}

unsafe fn c_topic(topic: *const c_char) -> String {
    CStr::from_ptr(topic).to_string_lossy().into_owned()
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_read(
    ctx: *mut KosThreadContext,
    topic: *const c_char,
) -> *const c_void {
    if ctx.is_null() || topic.is_null() {
        return ptr::null();
    }
    let ctx = thread_ctx(ctx);
    let topic = c_topic(topic);
    if !ctx.subscribed_topics().contains(&topic) {
        return ptr::null();
    }
    let bytes = ctx.read_bytes(&topic);
    if bytes.is_empty() {
        ptr::null()
    } else {
        bytes.as_ptr() as *const c_void
    }
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_read_size(ctx: *mut KosThreadContext, topic: *const c_char) -> u32 {
    if ctx.is_null() || topic.is_null() {
        return 0;
    }
    let ctx = thread_ctx(ctx);
    let topic = c_topic(topic);
    if !ctx.subscribed_topics().contains(&topic) {
        return 0;
    }
    ctx.read_bytes(&topic).len() as u32
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_is_fresh(ctx: *mut KosThreadContext, topic: *const c_char) -> c_int {
    if ctx.is_null() || topic.is_null() {
        return 0;
    }
    thread_ctx(ctx).is_fresh(&c_topic(topic)) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_try_read(
    ctx: *mut KosThreadContext,
    topic: *const c_char,
) -> *const c_void {
    kos_ctx_read(ctx, topic)
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_read_ago(
    ctx: *mut KosThreadContext,
    topic: *const c_char,
    ago: u32,
) -> *const c_void {
    kos_ctx_read_ago_bytes(ctx, topic, ago).map_or(ptr::null(), |b| b.as_ptr() as *const c_void)
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_read_ago_size(
    ctx: *mut KosThreadContext,
    topic: *const c_char,
    ago: u32,
) -> u32 {
    kos_ctx_read_ago_bytes(ctx, topic, ago).map_or(0, |b| b.len() as u32)
}

unsafe fn kos_ctx_read_ago_bytes<'a>(
    ctx: *mut KosThreadContext,
    topic: *const c_char,
    ago: u32,
) -> Option<&'a [u8]> {
    if ctx.is_null() || topic.is_null() {
        return None;
    }
    let ctx = thread_ctx(ctx);
    let topic = c_topic(topic);
    if !ctx.subscribed_topics().contains(&topic) {
        return None;
    }
    ctx.read_ago_bytes(&topic, ago as usize).filter(|b| !b.is_empty())
}

#[no_mangle]
pub unsafe extern "C" fn kos_ctx_write(
    ctx: *mut KosThreadContext,
    topic: *const c_char,
    data: *const c_void,
    size: u32,
) -> c_int {
    if ctx.is_null() || topic.is_null() || data.is_null() || size == 0 {
        return KOS_ERROR;
    }
    let ctx = thread_ctx(ctx);
    let topic = c_topic(topic);
    if !ctx.published_topics().contains(&topic) {
        return KOS_ERROR;
    }
    let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
    ctx.write_bytes(&topic, bytes);
    KOS_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::CString;

    unsafe extern "C" fn test_on_init(data: *mut c_void, ctx: *mut KosContext) -> c_int {
        let counter = &mut *(data as *mut u32);
        *counter += 1;
        kos_log_info(ctx, c"init called".as_ptr());
        KOS_OK
    }

    unsafe extern "C" fn test_on_run(data: *mut c_void, _ctx: *mut KosContext) -> c_int {
        let counter = &mut *(data as *mut u32);
        *counter += 1;
        KOS_OK
    }

    unsafe extern "C" fn test_on_run_fail(_data: *mut c_void, _ctx: *mut KosContext) -> c_int {
        KOS_ERROR
    }

    unsafe extern "C" fn test_on_error_terminate(
        _data: *mut c_void,
        _ctx: *mut KosContext,
        _err: *const KosErrorC,
    ) -> c_int {
        KOS_ACTION_TERMINATE
    }

    #[test]
    fn c_callbacks_invoked_via_bridge() {
        let mut counter: u32 = 0;
        let data_ptr = &mut counter as *mut u32 as *mut c_void;

        let cbs = KosAppCallbacks {
            on_init: Some(test_on_init),
            on_run: Some(test_on_run),
            on_suspend: None,
            on_resume: None,
            on_terminate: None,
            on_error: None,
        };

        let mut app = CApp {
            data: data_ptr,
            callbacks: cbs,
        };

        let mut ctx = AppContext::new("test", "default", HashMap::new());
        app.on_init(&mut ctx).unwrap();
        app.on_run(&mut ctx).unwrap();
        app.on_run(&mut ctx).unwrap();

        assert_eq!(counter, 3);
    }

    #[test]
    fn null_callbacks_use_defaults() {
        let mut counter: u32 = 0;
        let data_ptr = &mut counter as *mut u32 as *mut c_void;

        let cbs = KosAppCallbacks {
            on_init: Some(test_on_init),
            on_run: Some(test_on_run),
            on_suspend: None,
            on_resume: None,
            on_terminate: None,
            on_error: None,
        };

        let mut app = CApp {
            data: data_ptr,
            callbacks: cbs,
        };

        let mut ctx = AppContext::new("test", "default", HashMap::new());

        assert!(app.on_suspend(&mut ctx).is_ok());
        assert!(app.on_resume(&mut ctx).is_ok());
        assert!(app.on_terminate(&mut ctx).is_ok());

        let action = app.on_error(&mut ctx, &KosError::NotFound("x".into()));
        assert_eq!(action, ErrorAction::Restart);
    }

    #[test]
    fn c_callback_error_returns_kos_error() {
        let mut dummy: u32 = 0;
        let data_ptr = &mut dummy as *mut u32 as *mut c_void;

        let cbs = KosAppCallbacks {
            on_init: Some(test_on_run_fail),
            on_run: None,
            on_suspend: None,
            on_resume: None,
            on_terminate: None,
            on_error: None,
        };

        let mut app = CApp {
            data: data_ptr,
            callbacks: cbs,
        };

        let mut ctx = AppContext::new("test", "default", HashMap::new());
        let result = app.on_init(&mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn on_error_callback_terminate() {
        let mut dummy: u32 = 0;
        let data_ptr = &mut dummy as *mut u32 as *mut c_void;

        let cbs = KosAppCallbacks {
            on_init: None,
            on_run: None,
            on_suspend: None,
            on_resume: None,
            on_terminate: None,
            on_error: Some(test_on_error_terminate),
        };

        let mut app = CApp {
            data: data_ptr,
            callbacks: cbs,
        };

        let mut ctx = AppContext::new("test", "default", HashMap::new());
        let action = app.on_error(&mut ctx, &KosError::NotFound("x".into()));
        assert_eq!(action, ErrorAction::Terminate);
    }

    #[test]
    fn context_identity_ffi() {
        let mut wrapper = KosContext {
            inner: AppContext::new("my-app", "adas", HashMap::new()),
            string_cache: Vec::new(),
            transport: None,
        };

        unsafe {
            let id = kos_app_id(&mut wrapper);
            assert!(!id.is_null());
            assert_eq!(CStr::from_ptr(id).to_str().unwrap(), "my-app");

            let dom = kos_domain(&mut wrapper);
            assert!(!dom.is_null());
            assert_eq!(CStr::from_ptr(dom).to_str().unwrap(), "adas");
        }
    }

    #[test]
    fn context_param_ffi() {
        let mut params = HashMap::new();
        params.insert("gain".into(), "1.5".into());
        params.insert("count".into(), "42".into());

        let mut wrapper = KosContext {
            inner: AppContext::new("app", "d", params),
            string_cache: Vec::new(),
            transport: None,
        };

        unsafe {
            let gain_key = CString::new("gain").unwrap();
            let v = kos_param_float(&mut wrapper, gain_key.as_ptr(), 0.0);
            assert!((v - 1.5).abs() < f64::EPSILON);

            let count_key = CString::new("count").unwrap();
            let v = kos_param_int(&mut wrapper, count_key.as_ptr(), 0);
            assert_eq!(v, 42);

            let missing_key = CString::new("nope").unwrap();
            let v = kos_param_float(&mut wrapper, missing_key.as_ptr(), 99.0);
            assert!((v - 99.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn null_pointers_handled_safely() {
        unsafe {
            assert!(kos_app_id(ptr::null_mut()).is_null());
            assert!(kos_domain(ptr::null_mut()).is_null());
            assert!((kos_param_float(ptr::null_mut(), ptr::null(), 1.0) - 1.0).abs() < f64::EPSILON);
            assert_eq!(kos_param_int(ptr::null_mut(), ptr::null(), 7), 7);
            kos_log_info(ptr::null_mut(), ptr::null());
            kos_log_warn(ptr::null_mut(), ptr::null());
            kos_log_error(ptr::null_mut(), ptr::null());
        }
    }

    #[test]
    fn node_create_and_destroy() {
        unsafe {
            let name = CString::new("test_node").unwrap();
            let node = kos_node_new(name.as_ptr());
            assert!(!node.is_null());
            kos_node_destroy(node);
        }
    }

    #[test]
    fn node_null_name_returns_null() {
        unsafe {
            let node = kos_node_new(ptr::null());
            assert!(node.is_null());
        }
    }

    #[test]
    fn node_destroy_null_safe() {
        unsafe {
            kos_node_destroy(ptr::null_mut());
        }
    }

    #[test]
    fn node_shutdown_null_safe() {
        unsafe {
            kos_node_shutdown(ptr::null_mut());
        }
    }

    #[test]
    fn create_thread_null_params_returns_error() {
        unsafe {
            let result = kos_create_thread(
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
            );
            assert_eq!(result, KOS_ERROR);
        }
    }

    #[test]
    fn ctx_read_null_safe() {
        unsafe {
            assert!(kos_ctx_read(ptr::null_mut(), ptr::null()).is_null());
            assert!(kos_ctx_try_read(ptr::null_mut(), ptr::null()).is_null());
            assert!(kos_ctx_read_ago(ptr::null_mut(), ptr::null(), 0).is_null());
        }
    }

    #[test]
    fn ctx_write_null_safe() {
        unsafe {
            assert_eq!(kos_ctx_write(ptr::null_mut(), ptr::null(), ptr::null(), 0), KOS_ERROR);
        }
    }

    #[test]
    fn ctx_write_and_read() {
        let mut ctx = kos_exec::ThreadContext::new("test", vec!["input".into()], vec!["output".into()]);
        ctx.inject_topic_data("input", 7u32.to_ne_bytes().to_vec());
        let c = as_c_ctx(&ctx);

        unsafe {
            let output = CString::new("output").unwrap();
            let data: u32 = 42;
            let size = std::mem::size_of::<u32>() as u32;
            assert_eq!(kos_ctx_write(c, output.as_ptr(), &data as *const u32 as *const c_void, size), KOS_OK);

            let input = CString::new("input").unwrap();
            let p = kos_ctx_read(c, input.as_ptr()) as *const u32;
            assert!(!p.is_null());
            assert_eq!(p.read_unaligned(), 7);
            assert_eq!(kos_ctx_read_size(c, input.as_ptr()), 4);

            let other = CString::new("other").unwrap();
            assert!(kos_ctx_read(c, other.as_ptr()).is_null());
            assert_eq!(kos_ctx_write(c, other.as_ptr(), &data as *const u32 as *const c_void, size), KOS_ERROR);
        }
        assert_eq!(ctx.drain_writes().get("output").map(|v| v.len()), Some(4));
    }

    #[test]
    fn priority_constants() {
        assert_eq!(c_priority_to_priority(KOS_CRITICAL), kos_exec::Priority::Critical);
        assert_eq!(c_priority_to_priority(KOS_HIGH), kos_exec::Priority::High);
        assert_eq!(c_priority_to_priority(KOS_NORMAL), kos_exec::Priority::Normal);
        assert_eq!(c_priority_to_priority(KOS_LOW), kos_exec::Priority::Low);
        assert_eq!(c_priority_to_priority(99), kos_exec::Priority::Normal);
    }

    unsafe extern "C" fn upper_service(
        user_data: *mut c_void,
        req: *const c_void,
        req_size: usize,
        resp: *mut c_void,
        resp_capacity: usize,
        resp_size: *mut usize,
    ) -> c_int {
        let calls = &*(user_data as *const std::sync::atomic::AtomicU32);
        calls.fetch_add(1, Ordering::SeqCst);
        let req = std::slice::from_raw_parts(req as *const u8, req_size);
        if req == b"fail" {
            return KOS_ERROR;
        }
        let out = std::slice::from_raw_parts_mut(resp as *mut u8, resp_capacity);
        for (o, i) in out.iter_mut().zip(req) {
            *o = i.to_ascii_uppercase();
        }
        *resp_size = req_size;
        KOS_OK
    }

    #[test]
    fn service_advertise_and_call() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let name = CString::new(format!("ffi/upper_{}", std::process::id())).unwrap();
        unsafe {
            let svc = kos_advertise_service(name.as_ptr(), Some(upper_service), &calls as *const _ as *mut c_void);
            assert!(!svc.is_null());

            let mut buf = [0u8; 16];
            let mut len = 0usize;
            let rc = kos_call_service(ptr::null_mut(), name.as_ptr(), b"hello".as_ptr().cast(), 5,
                buf.as_mut_ptr().cast(), buf.len(), &mut len, 0);
            assert_eq!(rc, KOS_OK);
            assert_eq!(&buf[..len], b"HELLO");

            let mut small = [0u8; 2];
            let rc = kos_call_service(ptr::null_mut(), name.as_ptr(), b"hello".as_ptr().cast(), 5,
                small.as_mut_ptr().cast(), small.len(), &mut len, 0);
            assert_eq!(rc, KOS_ERR_TRUNCATED);
            assert_eq!(len, 5);

            let rc = kos_call_service(ptr::null_mut(), name.as_ptr(), b"fail".as_ptr().cast(), 4,
                buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut(), 0);
            assert_eq!(rc, KOS_ERR_REMOTE);

            kos_service_destroy(svc);
            let rc = kos_call_service(ptr::null_mut(), name.as_ptr(), b"x".as_ptr().cast(), 1,
                buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut(), 100);
            assert_eq!(rc, KOS_ERR_NOT_FOUND);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
