// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

#define _DEFAULT_SOURCE
#include "kos_exec.h"
#include <stdio.h>
#include <pthread.h>
#include <unistd.h>
#include <string.h>

typedef struct { uint32_t seq; double value; } Sample;

static uint32_t produced = 0, consumed = 0, last_seq = 0, out_of_order = 0, history_ok = 0;

static void producer_run(KosThreadContext* ctx, void* ud) {
    (void)ud;
    produced++;
    Sample s = { produced, produced * 1.5 };
    kos_ctx_write(ctx, "ctest/sample", &s, sizeof s);
}
static void consumer_run(KosThreadContext* ctx, void* ud) {
    (void)ud;
    if (!kos_ctx_is_fresh(ctx, "ctest/sample")) return;
    const Sample* s = kos_ctx_read(ctx, "ctest/sample");
    if (!s || kos_ctx_read_size(ctx, "ctest/sample") != sizeof(Sample)) return;
    if (s->seq <= last_seq) out_of_order++;
    last_seq = s->seq;
    consumed++;
    const Sample* prev = kos_ctx_read_ago(ctx, "ctest/sample", 1);
    if (prev && kos_ctx_read_ago_size(ctx, "ctest/sample", 1) == sizeof(Sample) && prev->seq + 1 == s->seq) history_ok++;
}
static void* stopper(void* node) { usleep(300 * 1000); kos_node_shutdown(node); return NULL; }

static KosPublisher* pub; static KosSubscriber* sub; static int runs = 0, recv_ok = 0;
static KosResult app_init(void* d, KosContext* ctx) {
    (void)d;
    pub = kos_advertise(ctx, "ctest/app", sizeof(uint32_t));
    sub = kos_subscribe(ctx, "ctest/app", 1, sizeof(uint32_t));
    return (pub && sub) ? KOS_OK : KOS_ERROR;
}
static KosResult app_run(void* d, KosContext* ctx) {
    (void)d; (void)ctx;
    uint32_t v = (uint32_t)++runs;
    kos_publish(pub, &v);
    KosRecvResult* r = kos_recv(sub);
    if (r && r->len == sizeof v && memcmp(r->data, &v, sizeof v) == 0) recv_ok++;
    kos_recv_free(r);
    return runs >= 20 ? KOS_ERROR : KOS_OK;
}
static KosErrorAction app_error(void* d, KosContext* c, const KosError* e) { (void)d; (void)c; (void)e; return KOS_ACTION_TERMINATE; }

int main(void) {
    KosNode* node = kos_node_new("ctest");
    const char* topics[] = { "ctest/sample" };
    KosThreadConfig pcfg = { KOS_PERIODIC, 5, NULL, KOS_NORMAL, -1, NULL, 0, topics, 1 };
    KosThreadConfig ccfg = { KOS_EVENT, 0, "ctest/sample", KOS_NORMAL, -1, topics, 1, NULL, 0 };
    KosThreadCallbacks pcb = { NULL, producer_run, NULL, NULL };
    KosThreadCallbacks ccb = { NULL, consumer_run, NULL, NULL };
    if (kos_create_thread(node, "producer", &pcfg, &pcb) || kos_create_thread(node, "consumer", &ccfg, &ccb)) {
        puts("create_thread failed"); return 1;
    }
    pthread_t t; pthread_create(&t, NULL, stopper, node);
    int rc = kos_node_spin(node);
    pthread_join(t, NULL);
    kos_node_destroy(node);
    printf("node: spin=%d produced=%u consumed=%u out_of_order=%u history_ok=%u\n", rc, produced, consumed, out_of_order, history_ok);

    KosAppCallbacks cb = { app_init, app_run, NULL, NULL, NULL, app_error };
    kos_app_run("ctest.app", NULL, 0, &cb);
    kos_publisher_destroy(pub); kos_subscriber_destroy(sub);
    printf("app: runs=%d recv_ok=%d\n", runs, recv_ok);
    return (consumed > 20 && out_of_order == 0 && history_ok + 1 >= consumed && recv_ok == 20) ? 0 : 1;
}
