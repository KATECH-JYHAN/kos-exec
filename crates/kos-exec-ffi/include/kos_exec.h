// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

#pragma once
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    KOS_OK              =  0,
    KOS_ERROR           = -1,
    KOS_ERR_TIMEOUT     = -2,
    KOS_ERR_NOT_FOUND   = -3,
    KOS_ERR_TRUNCATED   = -4,
    KOS_ERR_REMOTE      = -5,
} KosResult;

typedef enum {
    KOS_ACTION_RESTART   = 0,
    KOS_ACTION_TERMINATE = 1,
    KOS_ACTION_IGNORE    = 2,
} KosErrorAction;

typedef struct {
    int         code;
    const char* message;
} KosError;

typedef struct KosContext KosContext;
typedef struct KosPublisher KosPublisher;
typedef struct KosSubscriber KosSubscriber;
typedef struct KosService KosService;

typedef KosResult      (*KosInitFn)     (void* data, KosContext* ctx);
typedef KosResult      (*KosRunFn)      (void* data, KosContext* ctx);
typedef KosResult      (*KosSuspendFn)  (void* data, KosContext* ctx);
typedef KosResult      (*KosResumeFn)   (void* data, KosContext* ctx);
typedef KosResult      (*KosTerminateFn)(void* data, KosContext* ctx);
typedef KosErrorAction (*KosErrorFn)    (void* data, KosContext* ctx, const KosError* err);

typedef struct {
    KosInitFn      on_init;
    KosRunFn       on_run;
    KosSuspendFn   on_suspend;
    KosResumeFn    on_resume;
    KosTerminateFn on_terminate;
    KosErrorFn     on_error;
} KosAppCallbacks;

void kos_app_run(const char* app_id,
                 void* data,
                 size_t data_size,
                 const KosAppCallbacks* callbacks);

const char* kos_app_id(KosContext* ctx);
const char* kos_domain(KosContext* ctx);

double      kos_param_float(KosContext* ctx, const char* key, double default_val);
int64_t     kos_param_int  (KosContext* ctx, const char* key, int64_t default_val);
const char* kos_param_str  (KosContext* ctx, const char* key, const char* default_val);

void kos_log_info (KosContext* ctx, const char* msg);
void kos_log_warn (KosContext* ctx, const char* msg);
void kos_log_error(KosContext* ctx, const char* msg);

KosPublisher*  kos_advertise(KosContext* ctx, const char* topic, size_t msg_size);
KosSubscriber* kos_subscribe(KosContext* ctx, const char* topic, size_t depth, size_t msg_size);
KosResult      kos_publish  (KosPublisher* pub_handle, const void* data);

typedef struct {
    const uint8_t* data;
    size_t         len;
    size_t         _capacity;
} KosRecvResult;

KosRecvResult* kos_recv     (KosSubscriber* sub);
void           kos_recv_free(KosRecvResult* result);

void kos_publisher_destroy (KosPublisher* pub_handle);
void kos_subscriber_destroy(KosSubscriber* sub);

typedef int (*KosServiceFn)(void* user_data,
                            const void* req, size_t req_size,
                            void* resp, size_t resp_capacity, size_t* resp_size);

KosService* kos_advertise_service(const char* service, KosServiceFn handler, void* user_data);
void        kos_service_destroy  (KosService* svc);
KosResult   kos_call_service(KosContext* ctx, const char* service,
                             const void* req, size_t req_size,
                             void* resp, size_t resp_size, size_t* resp_len,
                             uint32_t timeout_ms);

typedef struct KosNode KosNode;
typedef struct KosThreadContext KosThreadContext;

enum { KOS_PERIODIC = 0, KOS_EVENT = 1 };
enum { KOS_CRITICAL = 0, KOS_HIGH = 1, KOS_NORMAL = 2, KOS_LOW = 3 };

typedef struct {
    int                trigger_type;
    uint32_t           period_ms;
    const char*        event_topic;
    int                priority;
    int                cpu_affinity;
    const char* const* subs;
    uint32_t           subs_count;
    const char* const* pubs;
    uint32_t           pubs_count;
} KosThreadConfig;

typedef struct {
    int  (*on_init)    (KosThreadContext* ctx, void* user_data);
    void (*on_run)     (KosThreadContext* ctx, void* user_data);
    void (*on_shutdown)(KosThreadContext* ctx, void* user_data);
    void* user_data;
} KosThreadCallbacks;

KosNode* kos_node_new(const char* name);
void     kos_node_destroy(KosNode* node);
int      kos_create_thread(KosNode* node, const char* name,
                           const KosThreadConfig* config,
                           const KosThreadCallbacks* callbacks);
int      kos_node_spin(KosNode* node);
void     kos_node_shutdown(KosNode* node);

const void* kos_ctx_read     (KosThreadContext* ctx, const char* topic);
uint32_t    kos_ctx_read_size(KosThreadContext* ctx, const char* topic);
int         kos_ctx_is_fresh (KosThreadContext* ctx, const char* topic);
const void* kos_ctx_try_read (KosThreadContext* ctx, const char* topic);
const void* kos_ctx_read_ago (KosThreadContext* ctx, const char* topic, uint32_t ago);
uint32_t    kos_ctx_read_ago_size(KosThreadContext* ctx, const char* topic, uint32_t ago);
int         kos_ctx_write    (KosThreadContext* ctx, const char* topic, const void* data, uint32_t size);

#ifdef __cplusplus
}
#endif
