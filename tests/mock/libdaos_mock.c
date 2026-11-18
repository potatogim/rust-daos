// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Stand-in for libdaos.so.2, used by tests/dlopen_mock.rs. It exports
// daos_init, daos_fini and (unless NO_REINIT) daos_reinit, the pool and
// container connect calls, plus the test controls shared with libdfs_mock.so:
// call counters, a call trace, fault injection, delays, a handle table, and a
// model of libdaos's per-thread private event.
//
// Handles: cookies from one counter, with a base per kind (pool 0x1000...,
// container 0x2000...). A call with an unknown or closed cookie returns
// -DER_NO_HDL. daos_cont_close returns -DER_BUSY while the container is
// mounted or has open objects, daos_pool_disconnect while the pool has open
// containers, so release-order mistakes fail.
//
// Names: pool "missing" and containers whose name starts with "missing" do
// not exist (-DER_NONEXIST); containers whose name starts with "plain" exist
// but are not POSIX containers. A name that is neither a created label nor
// the UUID string of a created container exists implicitly from its first
// open until it is destroyed.
//
// Property lists: daos_prop_alloc tags its allocations; daos_prop_free and
// daos_cont_query abort on a list that did not come from daos_prop_alloc.
//
// Build variants: -DNO_REINIT drops daos_reinit; -DNO_V2_NAMES exports only
// the plain names (daos_pool_connect, daos_cont_open) instead of the *2 names.
//
// The event model: daos_init with a zero count creates a new network context
// (generation++); daos_fini with the last reference destroys it. Every thread
// remembers the generation of its first DFS call, as libdaos keeps a
// thread-local "event initialised" flag that fini clears only on the calling
// thread. A thread whose remembered generation is not the live one gets EIO:
// that is what a finalised and re-initialised libdaos would do to it.
//
// This mock checks calls and error paths only. Layouts are checked by the
// ABI probe (tools/abi-probe) against the real headers.

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define MAX_OPS 64
#define TRACE_MAX (1 << 20)

struct op_state {
	char     name[32];
	unsigned long calls;
	int      fail_rc;
	unsigned fail_skip;
	unsigned fail_times;
	unsigned delay_ms;
};

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct op_state ops[MAX_OPS];
static int             nops;
static char            trace[TRACE_MAX];
static size_t          trace_len;
static int             init_count;
static unsigned        generation;
static int             context_alive;
static __thread unsigned thread_generation;

static struct op_state *
op_get(const char *name)
{
	for (int i = 0; i < nops; i++)
		if (strcmp(ops[i].name, name) == 0)
			return &ops[i];
	if (nops == MAX_OPS)
		abort();
	snprintf(ops[nops].name, sizeof(ops[nops].name), "%s", name);
	return &ops[nops++];
}

/*
 * Count and trace a call; return 1 and set *rc when a fault is injected.
 * Otherwise *rc is set to 0, so callers may return it on their success path.
 */
int
mock_enter(const char *name, int *rc)
{
	unsigned delay;
	int      injected = 0;

	*rc = 0;
	pthread_mutex_lock(&lock);
	struct op_state *s = op_get(name);
	s->calls++;
	size_t n = strlen(name);
	if (trace_len + n + 2 < TRACE_MAX) {
		memcpy(trace + trace_len, name, n);
		trace_len += n;
		trace[trace_len++] = '\n';
		trace[trace_len] = 0;
	}
	delay = s->delay_ms;
	if (s->fail_times > 0) {
		if (s->fail_skip > 0) {
			s->fail_skip--;
		} else {
			s->fail_times--;
			*rc      = s->fail_rc;
			injected = 1;
		}
	}
	pthread_mutex_unlock(&lock);
	if (delay)
		usleep(delay * 1000);
	return injected;
}

void
mock_set_fail(const char *name, int rc, unsigned skip, unsigned times)
{
	pthread_mutex_lock(&lock);
	struct op_state *s = op_get(name);
	s->fail_rc    = rc;
	s->fail_skip  = skip;
	s->fail_times = times;
	pthread_mutex_unlock(&lock);
}

void
mock_set_delay(const char *name, unsigned ms)
{
	pthread_mutex_lock(&lock);
	op_get(name)->delay_ms = ms;
	pthread_mutex_unlock(&lock);
}

unsigned long
mock_calls(const char *name)
{
	unsigned long n;

	pthread_mutex_lock(&lock);
	n = op_get(name)->calls;
	pthread_mutex_unlock(&lock);
	return n;
}

/* Copy the call trace ("name\n" per call) into buf; returns its length. */
size_t
mock_trace(char *buf, size_t len)
{
	size_t n;

	pthread_mutex_lock(&lock);
	n = trace_len < len ? trace_len : len;
	memcpy(buf, trace, n);
	pthread_mutex_unlock(&lock);
	return n;
}

/* Called by every libdfs_mock call that would use the thread's private event. */
int
mock_thread_event_check(void)
{
	int rc = 0;

	pthread_mutex_lock(&lock);
	if (!context_alive)
		rc = EIO;
	else if (thread_generation == 0)
		thread_generation = generation;
	else if (thread_generation != generation)
		rc = EIO;
	pthread_mutex_unlock(&lock);
	return rc;
}

int
mock_daos_initialized(void)
{
	int alive;

	pthread_mutex_lock(&lock);
	alive = context_alive;
	pthread_mutex_unlock(&lock);
	return alive;
}

int
daos_init(void)
{
	int rc;

	if (mock_enter("daos_init", &rc))
		return rc;
	pthread_mutex_lock(&lock);
	if (init_count++ == 0) {
		generation++;
		context_alive = 1;
	}
	pthread_mutex_unlock(&lock);
	return 0;
}

int
daos_fini(void)
{
	int rc;

	if (mock_enter("daos_fini", &rc))
		return rc;
	pthread_mutex_lock(&lock);
	if (init_count > 0 && --init_count == 0) {
		context_alive = 0;
		/* Clears only the calling thread's flag, like libdaos. */
		thread_generation = 0;
	}
	pthread_mutex_unlock(&lock);
	return 0;
}

#ifndef NO_REINIT
int
daos_reinit(void)
{
	int rc;

	if (mock_enter("daos_reinit", &rc))
		return rc;
	return 0;
}
#endif

/* ------------------------------------------------------------------------ */
/* Handles and containers */

typedef struct {
	uint64_t cookie;
} daos_handle_t;
typedef unsigned char uuid_t[16];

struct daos_prop_entry {
	uint32_t dpe_type;
	uint16_t dpe_flags;
	uint16_t dpe_reserv;
	uint64_t dpe_val;
};
typedef struct {
	uint32_t                dpp_nr;
	uint32_t                dpp_reserv;
	struct daos_prop_entry *dpp_entries;
} daos_prop_t;
typedef struct {
	uint64_t epr_lo;
	uint64_t epr_hi;
} daos_epoch_range_t;
struct daos_space {
	uint64_t s_total[2];
	uint64_t s_free[2];
};
typedef struct {
	unsigned char pi_uuid[16];
	uint32_t      pi_ntargets, pi_nnodes, pi_ndisabled, pi_map_ver, pi_leader;
	uint64_t      pi_bits;
	struct {
		struct daos_space ps_space;
		uint64_t          ps_free_min[2], ps_free_max[2], ps_free_mean[2];
		uint32_t          ps_ntargets, ps_padding;
	} pi_space;
	struct {
		uint32_t rs_version, rs_seconds;
		int32_t  rs_errno, rs_state, rs_reserved, rs_fail_rank;
		uint64_t rs_toberb_obj_nr, rs_obj_nr, rs_rec_nr, rs_size;
	} pi_rebuild_st;
} daos_pool_info_t;
typedef struct {
	uint32_t          ta_reserved;
	int32_t           ta_state;
	int32_t           ta_perf;
	uint32_t          ta_pad;
	struct daos_space ta_space;
} daos_target_info_t;
typedef struct {
	unsigned char ci_uuid[16];
	uint64_t      ci_lsnapshot;
	uint32_t      ci_nhandles, ci_nsnapshots;
	uint64_t      ci_md_otime, ci_md_mtime;
} daos_cont_info_t;
struct daos_pool_cont_info {
	unsigned char pci_uuid[16];
	char          pci_label[128];
};

#define DER_NO_PERM  1001
#define DER_NO_HDL   1002
#define DER_INVAL    1003
#define DER_EXIST    1004
#define DER_NONEXIST 1005
#define DER_NOSYS    1010
#define DER_BUSY     1012
#define DER_TRUNC    1016
#define DER_IO       2001

#define DAOS_PC_RW  2u
#define DAOS_COO_RW 2u

#define PROP_LABEL       4097
#define PROP_LAYOUT_TYPE 4098
#define PROP_REDUN_FAC   4103
#define PROP_OWNER       4109
#define PROP_ROOTS       4113
#define PROP_STATUS      4114

#define POOL_BASE   0x1000000000ull
#define CONT_BASE   0x2000000000ull
#define MAX_HANDLES 4096
#define MAX_CONTS   256
#define MAX_PROPS   16
#define MAX_SNAPS   64

enum hkind { H_FREE = 0, H_POOL, H_CONT };

struct handle {
	enum hkind kind;
	uint64_t   cookie;
	uint64_t   parent; /* pool cookie of a container handle */
	int        cont;   /* index of the container */
	int        rw;
	long       users; /* containers of a pool; mounts and objects of a container */
};

struct container {
	int           used;
	unsigned char uuid[16];
	char          label[128];
	int           posix;
	long          handles;
	int           nprops;
	uint32_t      ptype[MAX_PROPS];
	uint64_t      pval[MAX_PROPS];
	int           nsnap;
	uint64_t      snaps[MAX_SNAPS];
};

static struct handle    handles[MAX_HANDLES];
static struct container conts[MAX_CONTS];
static char             destroyed[MAX_CONTS][128];
static int              ndestroyed;
static uint64_t         next_cookie = 1;
static uint64_t         next_uuid   = 1;
static uint64_t         next_epoch  = 1000;
static void            *prop_lists[256];
static long             live_props;

static struct handle *
h_find(uint64_t cookie, enum hkind kind)
{
	for (int i = 0; i < MAX_HANDLES; i++)
		if (handles[i].kind == kind && handles[i].cookie == cookie)
			return &handles[i];
	return NULL;
}

static struct handle *
h_new(enum hkind kind, uint64_t base)
{
	for (int i = 0; i < MAX_HANDLES; i++)
		if (handles[i].kind == H_FREE) {
			memset(&handles[i], 0, sizeof(handles[i]));
			handles[i].kind   = kind;
			handles[i].cookie = base + next_cookie++;
			return &handles[i];
		}
	abort();
}

static int
parse_uuid(const char *s, unsigned char out[16])
{
	int n = 0;

	if (strlen(s) != 36)
		return 0;
	for (int i = 0; i < 36;) {
		if (i == 8 || i == 13 || i == 18 || i == 23) {
			if (s[i] != '-')
				return 0;
			i++;
			continue;
		}
		unsigned v;
		if (sscanf(s + i, "%2x", &v) != 1)
			return 0;
		out[n++] = (unsigned char)v;
		i += 2;
	}
	return 1;
}

static int
c_find(const char *name)
{
	unsigned char u[16];
	int           is_uuid = parse_uuid(name, u);

	for (int i = 0; i < MAX_CONTS; i++) {
		if (!conts[i].used)
			continue;
		if (is_uuid ? memcmp(conts[i].uuid, u, 16) == 0 : strcmp(conts[i].label, name) == 0)
			return i;
	}
	return -1;
}

static int
c_new(const char *label, int posix)
{
	for (int i = 0; i < MAX_CONTS; i++)
		if (!conts[i].used) {
			memset(&conts[i], 0, sizeof(conts[i]));
			conts[i].used = 1;
			snprintf(conts[i].label, sizeof(conts[i].label), "%s", label ? label : "");
			conts[i].posix = posix;
			memcpy(conts[i].uuid, "memcont-", 8);
			uint64_t n = next_uuid++;
			for (int b = 0; b < 8; b++)
				conts[i].uuid[15 - b] = (unsigned char)(n >> (8 * b));
			for (int d = 0; d < ndestroyed; d++)
				if (label && strcmp(destroyed[d], label) == 0)
					destroyed[d][0] = 0;
			return i;
		}
	abort();
}

/* Resolve a container name; -1 with *rc set when it does not exist. */
static int
c_resolve(const char *name, int *rc)
{
	unsigned char u[16];
	int           i = c_find(name);

	if (i >= 0)
		return i;
	if (strncmp(name, "missing", 7) == 0 || parse_uuid(name, u)) {
		*rc = -DER_NONEXIST;
		return -1;
	}
	for (int d = 0; d < ndestroyed; d++)
		if (strcmp(destroyed[d], name) == 0) {
			*rc = -DER_NONEXIST;
			return -1;
		}
	return c_new(name, strncmp(name, "plain", 5) != 0);
}

/* Number of live handles of a kind (1 = pool, 2 = container). */
long
mock_live_handles(int kind)
{
	long n = 0;

	pthread_mutex_lock(&lock);
	for (int i = 0; i < MAX_HANDLES; i++)
		if ((int)handles[i].kind == kind)
			n++;
	pthread_mutex_unlock(&lock);
	return n;
}

/* Property lists from daos_prop_alloc not yet freed. */
long
mock_live_props(void)
{
	long n;

	pthread_mutex_lock(&lock);
	n = live_props;
	pthread_mutex_unlock(&lock);
	return n;
}

/* Number of existing containers. */
long
mock_containers(void)
{
	long n = 0;

	pthread_mutex_lock(&lock);
	for (int i = 0; i < MAX_CONTS; i++)
		n += conts[i].used;
	pthread_mutex_unlock(&lock);
	return n;
}

/*
 * For libdfs_mock: check a pool and container pair and adjust the user count
 * of the container handle (delta +1 on mount, -1 on unmount). Returns 0, or
 * an errno as dfs_mount would. *posix and *rw describe the container handle.
 */
int
mock_cont_use(daos_handle_t poh, daos_handle_t coh, int delta, int *posix, int *rw)
{
	int rc = 0;

	pthread_mutex_lock(&lock);
	struct handle *c = h_find(coh.cookie, H_CONT);
	if (c == NULL || (delta >= 0 && (h_find(poh.cookie, H_POOL) == NULL || c->parent != poh.cookie))) {
		rc = EINVAL;
	} else {
		c->users += delta;
		if (posix)
			*posix = conts[c->cont].posix;
		if (rw)
			*rw = c->rw;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

/* Thread event check for daos_* calls: a stale event is -DER_IO. */
static int
event_der(void)
{
	return mock_thread_event_check() ? -DER_IO : 0;
}

/*
 * Create a container (for daos_cont_create* and dfs_cont_create*). A POSIX
 * create must not carry layout, label or roots entries in its list.
 */
int
mock_cont_create(daos_handle_t poh, const char *label, daos_prop_t *prop, unsigned char *uuid,
		 int posix)
{
	int rc = 0;

	pthread_mutex_lock(&lock);
	struct handle *p = h_find(poh.cookie, H_POOL);
	if (p == NULL) {
		rc = -DER_NO_HDL;
	} else if (!p->rw) {
		rc = -DER_NO_PERM;
	} else if (label && (label[0] == 0 || strlen(label) > 127 || parse_uuid(label, (unsigned char[16]){0}))) {
		rc = -DER_INVAL;
	} else if (label && c_find(label) >= 0) {
		rc = -DER_EXIST;
	} else if (prop && prop->dpp_nr > MAX_PROPS) {
		rc = -DER_INVAL;
	} else {
		uint64_t layout = posix ? 1 : 0;
		for (uint32_t i = 0; prop && i < prop->dpp_nr; i++) {
			uint32_t t = prop->dpp_entries[i].dpe_type;
			if (posix && (t == PROP_LAYOUT_TYPE || t == PROP_LABEL || t == PROP_ROOTS))
				rc = -DER_INVAL;
			if (!posix && t == PROP_LAYOUT_TYPE)
				layout = prop->dpp_entries[i].dpe_val;
		}
		if (rc == 0) {
			int c = c_new(label, posix);
			for (uint32_t i = 0; prop && i < prop->dpp_nr; i++) {
				if (prop->dpp_entries[i].dpe_type == PROP_LAYOUT_TYPE)
					continue;
				conts[c].ptype[conts[c].nprops]  = prop->dpp_entries[i].dpe_type;
				conts[c].pval[conts[c].nprops++] = prop->dpp_entries[i].dpe_val;
			}
			conts[c].ptype[conts[c].nprops]  = PROP_LAYOUT_TYPE;
			conts[c].pval[conts[c].nprops++] = layout;
			if (uuid)
				memcpy(uuid, conts[c].uuid, 16);
		}
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

#ifdef NO_V2_NAMES
#define POOL_CONNECT_NAME daos_pool_connect
#define CONT_OPEN_NAME    daos_cont_open
#define CONT_CREATE_NAME  daos_cont_create
#define CONT_DESTROY_NAME daos_cont_destroy
#else
#define POOL_CONNECT_NAME daos_pool_connect2
#define CONT_OPEN_NAME    daos_cont_open2
#define CONT_CREATE_NAME  daos_cont_create2
#define CONT_DESTROY_NAME daos_cont_destroy2
#endif

int
POOL_CONNECT_NAME(const char *pool, const char *sys, unsigned int flags, daos_handle_t *poh,
		  daos_pool_info_t *info, void *ev)
{
	int rc;

	(void)sys;
	if (mock_enter("daos_pool_connect", &rc))
		return rc;
	if ((rc = event_der()))
		return rc;
	if (!pool || !poh || ev != NULL)
		return -DER_INVAL;
	if (strcmp(pool, "missing") == 0)
		return -DER_NONEXIST;
	pthread_mutex_lock(&lock);
	struct handle *h = h_new(H_POOL, POOL_BASE);
	h->rw            = (flags & DAOS_PC_RW) != 0;
	poh->cookie      = h->cookie;
	if (info)
		memcpy(info->pi_uuid, "mock-pool-uuid\0\1", 16);
	pthread_mutex_unlock(&lock);
	return 0;
}

int
daos_pool_disconnect(daos_handle_t poh, void *ev)
{
	int rc;

	if (mock_enter("daos_pool_disconnect", &rc))
		return rc;
	if ((rc = event_der()))
		return rc;
	if (ev != NULL)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	struct handle *h = h_find(poh.cookie, H_POOL);
	if (h == NULL)
		rc = -DER_NO_HDL;
	else if (h->users > 0)
		rc = -DER_BUSY;
	else
		h->kind = H_FREE;
	pthread_mutex_unlock(&lock);
	return rc;
}

int
CONT_OPEN_NAME(daos_handle_t poh, const char *cont, unsigned int flags, daos_handle_t *coh,
	       daos_cont_info_t *info, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_open", &rc))
		return rc;
	if ((rc = event_der()))
		return rc;
	if (!cont || !coh || ev != NULL)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	struct handle *p = h_find(poh.cookie, H_POOL);
	int            ci;
	if (p == NULL) {
		rc = -DER_NO_HDL;
	} else if ((flags & DAOS_COO_RW) && !p->rw) {
		rc = -DER_NO_PERM;
	} else if ((ci = c_resolve(cont, &rc)) >= 0) {
		struct handle *c = h_new(H_CONT, CONT_BASE);
		c->parent        = poh.cookie;
		c->cont          = ci;
		c->rw            = (flags & DAOS_COO_RW) != 0;
		conts[ci].handles++;
		p->users++;
		coh->cookie = c->cookie;
		if (info)
			memcpy(info->ci_uuid, conts[ci].uuid, 16);
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
daos_cont_close(daos_handle_t coh, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_close", &rc))
		return rc;
	if ((rc = event_der()))
		return rc;
	if (ev != NULL)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	struct handle *c = h_find(coh.cookie, H_CONT);
	if (c == NULL) {
		rc = -DER_NO_HDL;
	} else if (c->users > 0) {
		rc = -DER_BUSY;
	} else {
		struct handle *p = h_find(c->parent, H_POOL);
		if (p)
			p->users--;
		if (conts[c->cont].used && conts[c->cont].handles > 0)
			conts[c->cont].handles--;
		c->kind = H_FREE;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
daos_pool_query(daos_handle_t poh, void *ranks, daos_pool_info_t *info, void *pool_prop, void *ev)
{
	int rc;

	if (mock_enter("daos_pool_query", &rc))
		return rc;
	if ((rc = event_der()))
		return rc;
	if (ranks || pool_prop || ev || !info)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	if (!h_find(poh.cookie, H_POOL)) {
		rc = -DER_NO_HDL;
	} else {
		memcpy(info->pi_uuid, "mock-pool-uuid\0\1", 16);
		info->pi_ntargets                     = 4;
		info->pi_nnodes                       = 1;
		info->pi_space.ps_ntargets            = 4;
		info->pi_space.ps_space.s_total[1]    = 1ull << 34;
		info->pi_space.ps_space.s_free[1]     = 1ull << 33;
		info->pi_rebuild_st.rs_state          = 2;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
daos_pool_query_target(daos_handle_t poh, uint32_t tgt, uint32_t rank, daos_target_info_t *info,
		       void *ev)
{
	int rc;

	if (mock_enter("daos_pool_query_target", &rc))
		return rc;
	if (ev || !info)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	if (!h_find(poh.cookie, H_POOL))
		rc = -DER_NO_HDL;
	else if (tgt >= 4 || rank != 0)
		rc = -DER_NONEXIST;
	else
		info->ta_state = 4;
	pthread_mutex_unlock(&lock);
	return rc;
}

int
daos_pool_list_cont(daos_handle_t poh, uint64_t *ncont, struct daos_pool_cont_info *cbuf, void *ev)
{
	int rc;

	if (mock_enter("daos_pool_list_cont", &rc))
		return rc;
	if (ev || !ncont)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	if (!h_find(poh.cookie, H_POOL)) {
		rc = -DER_NO_HDL;
	} else {
		uint64_t n = 0, cap = *ncont;
		for (int i = 0; i < MAX_CONTS; i++) {
			if (!conts[i].used)
				continue;
			if (cbuf && n < cap) {
				memcpy(cbuf[n].pci_uuid, conts[i].uuid, 16);
				snprintf(cbuf[n].pci_label, 128, "%s", conts[i].label);
			}
			n++;
		}
		*ncont = n;
		if (cbuf && n > cap)
			rc = -DER_TRUNC;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
CONT_CREATE_NAME(daos_handle_t poh, unsigned char *uuid, daos_prop_t *prop, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_create", &rc))
		return rc;
	if (ev || !uuid)
		return -DER_INVAL;
	return mock_cont_create(poh, NULL, prop, uuid, 0);
}

int
daos_cont_create_with_label(daos_handle_t poh, const char *label, daos_prop_t *prop,
			    unsigned char *uuid, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_create_with_label", &rc))
		return rc;
	if (ev || !label)
		return -DER_INVAL;
	return mock_cont_create(poh, label, prop, uuid, 0);
}

int
CONT_DESTROY_NAME(daos_handle_t poh, const char *cont, int force, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_destroy", &rc))
		return rc;
	if (ev || !cont)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	int i = c_find(cont);
	if (!h_find(poh.cookie, H_POOL)) {
		rc = -DER_NO_HDL;
	} else if (i < 0) {
		rc = -DER_NONEXIST;
	} else if (conts[i].handles > 0 && !force) {
		rc = -DER_BUSY;
	} else {
		if (conts[i].label[0] && ndestroyed < MAX_CONTS)
			snprintf(destroyed[ndestroyed++], 128, "%s", conts[i].label);
		conts[i].used = 0;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

static int
prop_known(void *p)
{
	for (int i = 0; i < 256; i++)
		if (prop_lists[i] == p)
			return i;
	return -1;
}

daos_prop_t *
daos_prop_alloc(uint32_t nr)
{
	int rc;

	if (mock_enter("daos_prop_alloc", &rc))
		return NULL;
	daos_prop_t *p = calloc(1, sizeof(*p));
	if (nr) {
		p->dpp_nr      = nr;
		p->dpp_entries = calloc(nr, sizeof(*p->dpp_entries));
	}
	pthread_mutex_lock(&lock);
	for (int i = 0; i < 256; i++)
		if (prop_lists[i] == NULL) {
			prop_lists[i] = p;
			live_props++;
			break;
		}
	pthread_mutex_unlock(&lock);
	return p;
}

void
daos_prop_free(daos_prop_t *p)
{
	int rc, i;

	if (mock_enter("daos_prop_free", &rc) || p == NULL)
		return;
	pthread_mutex_lock(&lock);
	i = prop_known(p);
	if (i < 0) {
		fprintf(stderr, "daos_prop_free: foreign property list %p\n", (void *)p);
		abort();
	}
	prop_lists[i] = NULL;
	live_props--;
	pthread_mutex_unlock(&lock);
	for (uint32_t k = 0; k < p->dpp_nr; k++) {
		uint32_t t = p->dpp_entries[k].dpe_type;
		if (t == PROP_LABEL || t == PROP_OWNER)
			free((void *)(uintptr_t)p->dpp_entries[k].dpe_val);
	}
	free(p->dpp_entries);
	free(p);
}

int
daos_cont_query(daos_handle_t coh, daos_cont_info_t *info, daos_prop_t *prop, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_query", &rc))
		return rc;
	if ((rc = event_der()))
		return rc;
	if (ev)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	struct handle *c = h_find(coh.cookie, H_CONT);
	if (!c) {
		pthread_mutex_unlock(&lock);
		return -DER_NO_HDL;
	}
	if (prop && prop_known(prop) < 0) {
		fprintf(stderr, "daos_cont_query: property list not from daos_prop_alloc\n");
		abort();
	}
	struct container *ct = &conts[c->cont];
	if (info) {
		memcpy(info->ci_uuid, ct->uuid, 16);
		info->ci_nhandles   = (uint32_t)ct->handles;
		info->ci_nsnapshots = (uint32_t)ct->nsnap;
		info->ci_lsnapshot  = ct->nsnap ? ct->snaps[ct->nsnap - 1] : 0;
	}
	if (prop && prop->dpp_nr == 0) {
		int n            = ct->nprops + 3;
		prop->dpp_entries = calloc((size_t)n, sizeof(*prop->dpp_entries));
		prop->dpp_nr      = (uint32_t)n;
		prop->dpp_entries[0].dpe_type = PROP_LABEL;
		prop->dpp_entries[0].dpe_val  = (uint64_t)(uintptr_t)strdup(ct->label);
		prop->dpp_entries[1].dpe_type = PROP_OWNER;
		prop->dpp_entries[1].dpe_val  = (uint64_t)(uintptr_t)strdup("mock@");
		prop->dpp_entries[2].dpe_type = PROP_STATUS;
		for (int i = 0; i < ct->nprops; i++) {
			prop->dpp_entries[3 + i].dpe_type = ct->ptype[i];
			prop->dpp_entries[3 + i].dpe_val  = ct->pval[i];
		}
	}
	pthread_mutex_unlock(&lock);
	return 0;
}

int
daos_cont_create_snap(daos_handle_t coh, uint64_t *epoch, char *name, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_create_snap", &rc))
		return rc;
	if (ev)
		return -DER_INVAL;
	if (name)
		return -DER_NOSYS;
	pthread_mutex_lock(&lock);
	struct handle *c = h_find(coh.cookie, H_CONT);
	if (!c) {
		rc = -DER_NO_HDL;
	} else if (conts[c->cont].nsnap == MAX_SNAPS) {
		rc = -DER_INVAL;
	} else {
		next_epoch += 1000;
		conts[c->cont].snaps[conts[c->cont].nsnap++] = next_epoch;
		if (epoch)
			*epoch = next_epoch;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
daos_cont_list_snap(daos_handle_t coh, int *nr, uint64_t *epochs, char **names, void *anchor,
		    void *ev)
{
	int rc;

	(void)anchor;
	if (mock_enter("daos_cont_list_snap", &rc))
		return rc;
	if (ev || names || !nr)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	struct handle *c = h_find(coh.cookie, H_CONT);
	if (!c) {
		rc = -DER_NO_HDL;
	} else {
		struct container *ct = &conts[c->cont];
		for (int i = 0; epochs && i < ct->nsnap && i < *nr; i++)
			epochs[i] = ct->snaps[i];
		*nr = ct->nsnap;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
daos_cont_destroy_snap(daos_handle_t coh, daos_epoch_range_t epr, void *ev)
{
	int rc;

	if (mock_enter("daos_cont_destroy_snap", &rc))
		return rc;
	if (ev)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	struct handle *c = h_find(coh.cookie, H_CONT);
	if (!c) {
		rc = -DER_NO_HDL;
	} else {
		struct container *ct = &conts[c->cont];
		int               k  = 0;
		for (int i = 0; i < ct->nsnap; i++)
			if (ct->snaps[i] < epr.epr_lo || ct->snaps[i] > epr.epr_hi)
				ct->snaps[k++] = ct->snaps[i];
		rc        = k == ct->nsnap ? -DER_NONEXIST : 0;
		ct->nsnap = k;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

/* ------------------------------------------------------------------------ */
/* Objects, key-value, arrays, transactions                                 */
/*                                                                          */
/* Records: (container, oid, dkey, akey) -> a single value, or one record  */
/* of an array value per index. A transaction writes pending records       */
/* tagged with its handle; commit publishes them, abort and restart drop   */
/* them. Reads through a transaction see its own pending records first.     */
/* Build variants: -DNO_KV and -DNO_ARRAY leave out those groups.           */

typedef struct {
	uint64_t lo, hi;
} daos_obj_id_t;
typedef struct {
	void  *iov_buf;
	size_t iov_buf_len;
	size_t iov_len;
} d_iov_t;
typedef struct {
	uint32_t sg_nr;
	uint32_t sg_nr_out;
	d_iov_t *sg_iovs;
} d_sg_list_t;
typedef struct {
	uint64_t rx_idx, rx_nr;
} daos_recx_t;
typedef struct {
	d_iov_t      iod_name;
	uint32_t     iod_type;
	uint64_t     iod_size;
	uint64_t     iod_flags;
	uint32_t     iod_nr;
	daos_recx_t *iod_recxs;
} daos_iod_t;
typedef struct {
	uint64_t kd_key_len;
	uint32_t kd_val_type;
} daos_key_desc_t;
typedef struct {
	uint16_t da_type, da_shard;
	uint32_t da_flags;
	uint64_t da_sub_anchors;
	uint8_t  da_buf[104];
} daos_anchor_t;
typedef struct {
	uint64_t rg_idx, rg_len;
} daos_range_t;
typedef struct {
	uint64_t      arr_nr;
	daos_range_t *arr_rgs;
	uint64_t      arr_nr_short_read, arr_nr_read;
} daos_array_iod_t;
typedef struct {
	uint64_t st_size, st_max_epoch;
} daos_array_stbuf_t;

#define OBJ_BASE      0x3000000000ull
#define TX_BASE       0x4000000000ull
#define MAX_RECS      16384
#define MAX_KEY       256
#define MAX_OBJS      1024
#define MAX_TXS       256
#define DER_KEY2BIG   2012
#define DER_REC2BIG   2013
#define OO_RW         4u
#define TF_RDONLY     1u
#define TF_ZERO_COPY  2u
#define COND_DKEY_INS 2u
#define COND_DKEY_UPD 4u
#define COND_DKEY_GET 8u
#define COND_AKEY_INS 16u
#define COND_AKEY_UPD 32u
#define COND_AKEY_GET 64u
#define GET_MAX       1u
#define GET_DKEY      4u
#define GET_AKEY      8u
#define GET_RECX      16u
#define K_OBJ         1
#define K_KV          2
#define K_ARRAY       3

struct rec {
	int      used;
	int      cont;
	uint64_t lo, hi;
	uint8_t  dkey[MAX_KEY];
	size_t   dlen;
	uint8_t  akey[MAX_KEY];
	size_t   alen;
	int      single;
	uint64_t idx;   /* record index (array values) */
	uint64_t rsize; /* record size (array values) */
	uint8_t *val;
	size_t   vlen;
	uint64_t tx; /* 0 = committed */
	int      dead; /* pending punch */
};

struct objh {
	int      used;
	uint64_t cookie;
	uint64_t coh;
	int      cont;
	uint64_t lo, hi;
	int      rw;
	int      kind;
	uint64_t cell, chunk;
};

struct arrmeta {
	int      used;
	int      cont;
	uint64_t lo, hi;
	uint64_t cell, chunk, size, epoch;
};

/* libdaos transaction states (obj_tx.c, the same in 2.6.4 and 2.8.1). */
#define TXS_OPEN      0
#define TXS_COMMITTED 1
#define TXS_ABORTED   2
#define TXS_FAILED    3 /* after DER_TX_RESTART: only restart or close */
#define DER_ALREADY    1008
#define DER_TX_RESTART 2025

struct txh {
	int       used;
	int       status;
	uint64_t  cookie;
	int       cont;
	int       ro;
	uint64_t  snap;
	pthread_t owner;
	int       owner_set;
};

static struct rec     recs[MAX_RECS];
static struct objh    objs[MAX_OBJS];
static struct arrmeta arrays[MAX_OBJS];
static struct txh     txs[MAX_TXS];
static uint64_t       next_oid_lo = 1;
static uint64_t       epoch_now   = 1;
static long           cross_thread;
static int            key2big_times;
static long           key2big_report;

/* Commits from a thread other than the one that opened the transaction. */
long
mock_tx_cross_thread(void)
{
	long n;

	pthread_mutex_lock(&lock);
	n = cross_thread;
	pthread_mutex_unlock(&lock);
	return n;
}

/* Make the next `times` list calls return -DER_KEY2BIG reporting `report`
 * as the key length (negative: the buffer's own capacity, an exact fit). */
void
mock_set_key2big(int times, long report)
{
	pthread_mutex_lock(&lock);
	key2big_times  = times;
	key2big_report = report;
	pthread_mutex_unlock(&lock);
}

/* Live object, KV and array handles. */
long
mock_live_objs(void)
{
	long n = 0;

	pthread_mutex_lock(&lock);
	for (int i = 0; i < MAX_OBJS; i++)
		n += objs[i].used;
	pthread_mutex_unlock(&lock);
	return n;
}

/* Live transactions. */
long
mock_live_txs(void)
{
	long n = 0;

	pthread_mutex_lock(&lock);
	for (int i = 0; i < MAX_TXS; i++)
		n += txs[i].used;
	pthread_mutex_unlock(&lock);
	return n;
}

static int
otype(uint64_t hi)
{
	return (int)(hi >> 56);
}

static int
dkey_int(uint64_t hi)
{
	int t = otype(hi);
	return t == 2 || t == 4 || t == 9 || t == 11 || t == 12 || t == 13;
}

static int
akey_int(uint64_t hi)
{
	int t = otype(hi);
	return t == 3 || t == 4;
}

static struct objh *
o_find(uint64_t cookie)
{
	for (int i = 0; i < MAX_OBJS; i++)
		if (objs[i].used && objs[i].cookie == cookie)
			return &objs[i];
	return NULL;
}

static struct txh *
t_find(uint64_t cookie)
{
	for (int i = 0; i < MAX_TXS; i++)
		if (txs[i].used && txs[i].cookie == cookie)
			return &txs[i];
	return NULL;
}

static struct arrmeta *
a_find(int cont, uint64_t lo, uint64_t hi, int create)
{
	for (int i = 0; i < MAX_OBJS; i++)
		if (arrays[i].used && arrays[i].cont == cont && arrays[i].lo == lo && arrays[i].hi == hi)
			return &arrays[i];
	if (!create)
		return NULL;
	for (int i = 0; i < MAX_OBJS; i++)
		if (!arrays[i].used) {
			memset(&arrays[i], 0, sizeof(arrays[i]));
			arrays[i].used = 1;
			arrays[i].cont = cont;
			arrays[i].lo   = lo;
			arrays[i].hi   = hi;
			return &arrays[i];
		}
	abort();
}

/* Key bytes of an iov; integer keys must be 8 bytes at an aligned address. */
static int
key_of(const d_iov_t *iov, int is_int, const uint8_t **k, size_t *len)
{
	if (!iov || !iov->iov_buf || iov->iov_len == 0 || iov->iov_len > MAX_KEY ||
	    iov->iov_len > iov->iov_buf_len)
		return -DER_INVAL;
	if (is_int) {
		if (iov->iov_len != 8)
			return -DER_INVAL;
		if ((uintptr_t)iov->iov_buf % 8 != 0) {
			fprintf(stderr, "integer key at an unaligned address\n");
			abort();
		}
	}
	*k   = iov->iov_buf;
	*len = iov->iov_len;
	return 0;
}

/* Visible record: committed, or pending of `tx`; pending wins. */
static struct rec *
r_find(int cont, uint64_t lo, uint64_t hi, const uint8_t *d, size_t dl, const uint8_t *a, size_t al,
       int single, uint64_t idx, uint64_t tx)
{
	struct rec *committed = NULL;

	for (int i = 0; i < MAX_RECS; i++) {
		struct rec *r = &recs[i];
		if (!r->used || r->cont != cont || r->lo != lo || r->hi != hi || r->dlen != dl ||
		    memcmp(r->dkey, d, dl) || r->alen != al || memcmp(r->akey, a, al) ||
		    r->single != single || (!single && r->idx != idx))
			continue;
		if (tx && r->tx == tx)
			return r->dead ? NULL : r;
		if (r->tx == 0)
			committed = r;
	}
	return committed;
}

static void
r_put(int cont, uint64_t lo, uint64_t hi, const uint8_t *d, size_t dl, const uint8_t *a, size_t al,
      int single, uint64_t idx, uint64_t rsize, const void *val, size_t vlen, uint64_t tx, int dead)
{
	struct rec *slot = NULL;

	for (int i = 0; i < MAX_RECS; i++) {
		struct rec *r = &recs[i];
		if (r->used && r->tx == tx && r->cont == cont && r->lo == lo && r->hi == hi &&
		    r->dlen == dl && !memcmp(r->dkey, d, dl) && r->alen == al &&
		    !memcmp(r->akey, a, al) && r->single == single && (single || r->idx == idx)) {
			slot = r;
			free(r->val);
			break;
		}
	}
	for (int i = 0; !slot && i < MAX_RECS; i++)
		if (!recs[i].used)
			slot = &recs[i];
	if (!slot)
		abort();
	slot->used   = 1;
	slot->cont   = cont;
	slot->lo     = lo;
	slot->hi     = hi;
	slot->dlen   = dl;
	slot->alen   = al;
	slot->single = single;
	slot->idx    = idx;
	slot->rsize  = rsize;
	slot->tx     = tx;
	slot->dead   = dead;
	memcpy(slot->dkey, d, dl);
	memcpy(slot->akey, a, al);
	slot->vlen = vlen;
	slot->val  = vlen ? malloc(vlen) : NULL;
	if (vlen)
		memcpy(slot->val, val, vlen);
}

static void
r_drop(struct rec *r)
{
	free(r->val);
	memset(r, 0, sizeof(*r));
}

/* Remove committed records matching the filter (or mark them dead in a tx). */
static void
r_punch(int cont, uint64_t lo, uint64_t hi, const uint8_t *d, size_t dl, const uint8_t *a, size_t al,
	uint64_t tx)
{
	for (int i = 0; i < MAX_RECS; i++) {
		struct rec *r = &recs[i];
		if (!r->used || r->cont != cont || r->lo != lo || r->hi != hi)
			continue;
		if (d && (r->dlen != dl || memcmp(r->dkey, d, dl)))
			continue;
		if (a && (r->alen != al || memcmp(r->akey, a, al)))
			continue;
		if (tx && r->tx == 0)
			r_put(cont, lo, hi, r->dkey, r->dlen, r->akey, r->alen, r->single, r->idx,
			      r->rsize, NULL, 0, tx, 1);
		else if (r->tx == tx)
			r_drop(r);
	}
}

/* Check a data call: object handle, transaction, write permission. */
static int
data_check(daos_handle_t oh, daos_handle_t th, int write, struct objh **o)
{
	*o = o_find(oh.cookie);
	if (!*o)
		return -DER_NO_HDL;
	if (write && !(*o)->rw)
		return -DER_NO_PERM;
	if (th.cookie) {
		struct txh *t = t_find(th.cookie);
		if (!t || t->cont != (*o)->cont)
			return -DER_NO_HDL;
		if (write && t->ro)
			return -DER_NO_PERM;
		if (t->status != TXS_OPEN)
			return -DER_NO_PERM;
		if (!t->owner_set) {
			t->owner     = pthread_self();
			t->owner_set = 1;
		}
	}
	return 0;
}

static int
obj_open_kind(daos_handle_t coh, daos_obj_id_t oid, unsigned mode, daos_handle_t *oh, int kind,
	      uint64_t cell, uint64_t chunk)
{
	struct handle *c = h_find(coh.cookie, H_CONT);

	if (!c)
		return -DER_NO_HDL;
	if ((mode & OO_RW) && !c->rw)
		return -DER_NO_PERM;
	for (int i = 0; i < MAX_OBJS; i++)
		if (!objs[i].used) {
			objs[i].used   = 1;
			objs[i].cookie = OBJ_BASE + next_cookie++;
			objs[i].coh    = coh.cookie;
			objs[i].cont   = c->cont;
			objs[i].lo     = oid.lo;
			objs[i].hi     = oid.hi;
			objs[i].rw     = (mode & OO_RW) != 0;
			objs[i].kind   = kind;
			objs[i].cell   = cell;
			objs[i].chunk  = chunk;
			c->users++;
			oh->cookie = objs[i].cookie;
			return 0;
		}
	abort();
}

static int
obj_close_kind(daos_handle_t oh, int kind)
{
	struct objh *o = o_find(oh.cookie);

	if (!o)
		return -DER_NO_HDL;
	if (o->kind != kind)
		return -DER_INVAL;
	struct handle *c = h_find(o->coh, H_CONT);
	if (c)
		c->users--;
	o->used = 0;
	return 0;
}

#define LOCKED(expr)                                                                               \
	do {                                                                                       \
		pthread_mutex_lock(&lock);                                                         \
		int _r = (expr);                                                                   \
		pthread_mutex_unlock(&lock);                                                       \
		return _r;                                                                         \
	} while (0)

#define MOCK_ENTER(name)                                                                           \
	do {                                                                                       \
		int _rc;                                                                           \
		if (mock_enter(name, &_rc))                                                        \
			return _rc;                                                                \
		if ((_rc = event_der()))                                                           \
			return _rc;                                                                \
	} while (0)

int
daos_cont_alloc_oids(daos_handle_t coh, uint64_t num, uint64_t *oid, void *ev)
{
	MOCK_ENTER("daos_cont_alloc_oids");
	if (ev || !oid || !num)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	int rc = h_find(coh.cookie, H_CONT) ? 0 : -DER_NO_HDL;
	if (!rc) {
		*oid = next_oid_lo;
		next_oid_lo += num;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

#ifdef NO_V2_NAMES
#define GEN_OID_NAME daos_obj_generate_oid
#else
#define GEN_OID_NAME daos_obj_generate_oid2
#endif

int
GEN_OID_NAME(daos_handle_t coh, daos_obj_id_t *oid, uint32_t type, uint32_t cid, uint16_t hints,
	     uint32_t args)
{
	(void)args;
	MOCK_ENTER("daos_obj_generate_oid");
	if (!oid || type > 14 || type == 1)
		return -DER_INVAL;
	pthread_mutex_lock(&lock);
	int rc = h_find(coh.cookie, H_CONT) ? 0 : -DER_NO_HDL;
	pthread_mutex_unlock(&lock);
	if (rc)
		return rc;
	if (cid == 0)
		cid = (hints & 4) ? ((8u << 24) | 1) : ((1u << 24) | 1);
	oid->hi = (oid->hi & 0xffffffffull) | ((uint64_t)type << 56) | ((uint64_t)(cid >> 24) << 48) |
		  ((uint64_t)(cid & 0xffff) << 32);
	return 0;
}

int
daos_oclass_name2id(const char *name)
{
	int rc;

	if (mock_enter("daos_oclass_name2id", &rc))
		return rc;
	if (!strcmp(name, "S1"))
		return (1 << 24) | 1;
	if (!strcmp(name, "RP_2G1"))
		return (8 << 24) | 1;
	return 0;
}

int
daos_oclass_id2name(uint32_t id, char *name)
{
	int rc;

	if (mock_enter("daos_oclass_id2name", &rc))
		return rc;
	if (id == ((1u << 24) | 1))
		strcpy(name, "S1");
	else if (id == ((8u << 24) | 1))
		strcpy(name, "RP_2G1");
	else
		return -DER_NONEXIST;
	return 0;
}

int
daos_obj_open(daos_handle_t coh, daos_obj_id_t oid, unsigned mode, daos_handle_t *oh, void *ev)
{
	MOCK_ENTER("daos_obj_open");
	if (ev || !oh)
		return -DER_INVAL;
	LOCKED(obj_open_kind(coh, oid, mode, oh, K_OBJ, 0, 0));
}

int
daos_obj_close(daos_handle_t oh, void *ev)
{
	MOCK_ENTER("daos_obj_close");
	if (ev)
		return -DER_INVAL;
	LOCKED(obj_close_kind(oh, K_OBJ));
}

static int
update_locked(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey, unsigned nr,
	      daos_iod_t *iods, d_sg_list_t *sgls)
{
	struct objh   *o;
	const uint8_t *d, *a;
	size_t         dl, al;
	int            rc = data_check(oh, th, 1, &o);

	if (rc)
		return rc;
	if (!nr || !iods || !sgls)
		return -DER_INVAL;
	if ((rc = key_of(dkey, dkey_int(o->hi), &d, &dl)))
		return rc;
	int dexists = 0;
	for (int i = 0; i < MAX_RECS && !dexists; i++)
		dexists = recs[i].used && !recs[i].dead && recs[i].cont == o->cont &&
			  recs[i].lo == o->lo && recs[i].hi == o->hi && recs[i].dlen == dl &&
			  !memcmp(recs[i].dkey, d, dl) && (recs[i].tx == 0 || recs[i].tx == th.cookie);
	if ((flags & COND_DKEY_INS) && dexists)
		return -DER_EXIST;
	if ((flags & COND_DKEY_UPD) && !dexists)
		return -DER_NONEXIST;
	for (unsigned i = 0; i < nr; i++) {
		daos_iod_t *iod = &iods[i];
		if ((rc = key_of(&iod->iod_name, akey_int(o->hi), &a, &al)))
			return rc;
		if (sgls[i].sg_nr != 1 || !sgls[i].sg_iovs)
			return -DER_INVAL;
		d_iov_t *v = &sgls[i].sg_iovs[0];
		if (iod->iod_type == 1) {
			struct rec *ex = r_find(o->cont, o->lo, o->hi, d, dl, a, al, 1, 0, th.cookie);
			if ((flags & COND_AKEY_INS) && ex)
				return -DER_EXIST;
			if ((flags & COND_AKEY_UPD) && !ex)
				return -DER_NONEXIST;
			if (iod->iod_size != v->iov_len)
				return -DER_INVAL;
			r_put(o->cont, o->lo, o->hi, d, dl, a, al, 1, 0, 0, v->iov_buf, v->iov_len,
			      th.cookie, 0);
		} else if (iod->iod_type == 2) {
			uint64_t off = 0;
			for (uint32_t k = 0; k < iod->iod_nr; k++)
				for (uint64_t j = 0; j < iod->iod_recxs[k].rx_nr; j++) {
					if ((off + 1) * iod->iod_size > v->iov_len)
						return -DER_INVAL;
					r_put(o->cont, o->lo, o->hi, d, dl, a, al, 0,
					      iod->iod_recxs[k].rx_idx + j, iod->iod_size,
					      (uint8_t *)v->iov_buf + off * iod->iod_size, iod->iod_size,
					      th.cookie, 0);
					off++;
				}
		} else {
			return -DER_INVAL;
		}
	}
	return 0;
}

int
daos_obj_update(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey, unsigned nr,
		daos_iod_t *iods, d_sg_list_t *sgls, void *ev)
{
	MOCK_ENTER("daos_obj_update");
	if (ev)
		return -DER_INVAL;
	LOCKED(update_locked(oh, th, flags, dkey, nr, iods, sgls));
}

static int
fetch_locked(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey, unsigned nr,
	     daos_iod_t *iods, d_sg_list_t *sgls)
{
	struct objh   *o;
	const uint8_t *d, *a;
	size_t         dl, al;
	int            rc = data_check(oh, th, 0, &o), out = 0;

	if (rc)
		return rc;
	if (!nr || !iods)
		return -DER_INVAL;
	if ((rc = key_of(dkey, dkey_int(o->hi), &d, &dl)))
		return rc;
	if (flags & COND_DKEY_GET) {
		int found = 0;
		for (int i = 0; i < MAX_RECS && !found; i++)
			found = recs[i].used && !recs[i].dead && recs[i].cont == o->cont &&
				recs[i].lo == o->lo && recs[i].hi == o->hi && recs[i].dlen == dl &&
				!memcmp(recs[i].dkey, d, dl);
		if (!found)
			return -DER_NONEXIST;
	}
	for (unsigned i = 0; i < nr; i++) {
		daos_iod_t *iod = &iods[i];
		if ((rc = key_of(&iod->iod_name, akey_int(o->hi), &a, &al)))
			return rc;
		d_iov_t *v = sgls ? &sgls[i].sg_iovs[0] : NULL;
		if (iod->iod_type == 1) {
			struct rec *r = r_find(o->cont, o->lo, o->hi, d, dl, a, al, 1, 0, th.cookie);
			if ((flags & COND_AKEY_GET) && !r)
				return -DER_NONEXIST;
			iod->iod_size = r ? r->vlen : 0;
			if (v && r) {
				if (r->vlen > v->iov_buf_len)
					out = -DER_REC2BIG;
				else
					memcpy(v->iov_buf, r->val, r->vlen);
			}
		} else if (iod->iod_type == 2) {
			uint64_t off = 0, size = 0;
			for (uint32_t k = 0; k < iod->iod_nr; k++)
				for (uint64_t j = 0; j < iod->iod_recxs[k].rx_nr; j++) {
					struct rec *r =
					    r_find(o->cont, o->lo, o->hi, d, dl, a, al, 0,
						   iod->iod_recxs[k].rx_idx + j, th.cookie);
					if (r) {
						size = r->rsize;
						if (v && (off + 1) * r->rsize <= v->iov_buf_len)
							memcpy((uint8_t *)v->iov_buf + off * r->rsize,
							       r->val, r->rsize);
					}
					off++;
				}
			iod->iod_size = size;
		} else {
			return -DER_INVAL;
		}
	}
	return out;
}

int
daos_obj_fetch(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey, unsigned nr,
	       daos_iod_t *iods, d_sg_list_t *sgls, void *ioms, void *ev)
{
	MOCK_ENTER("daos_obj_fetch");
	if (ev || ioms)
		return -DER_INVAL;
	LOCKED(fetch_locked(oh, th, flags, dkey, nr, iods, sgls));
}

static int
punch_locked(daos_handle_t oh, daos_handle_t th, d_iov_t *dkeys, unsigned ndk, d_iov_t *dkey,
	     d_iov_t *akeys, unsigned nak)
{
	struct objh   *o;
	const uint8_t *d, *a;
	size_t         dl, al;
	int            rc = data_check(oh, th, 1, &o);

	if (rc)
		return rc;
	if (!dkeys && !dkey) {
		r_punch(o->cont, o->lo, o->hi, NULL, 0, NULL, 0, th.cookie);
		return 0;
	}
	for (unsigned i = 0; dkeys && i < ndk; i++) {
		if ((rc = key_of(&dkeys[i], dkey_int(o->hi), &d, &dl)))
			return rc;
		r_punch(o->cont, o->lo, o->hi, d, dl, NULL, 0, th.cookie);
	}
	if (dkey) {
		if ((rc = key_of(dkey, dkey_int(o->hi), &d, &dl)))
			return rc;
		for (unsigned i = 0; i < nak; i++) {
			if ((rc = key_of(&akeys[i], akey_int(o->hi), &a, &al)))
				return rc;
			r_punch(o->cont, o->lo, o->hi, d, dl, a, al, th.cookie);
		}
	}
	return 0;
}

int
daos_obj_punch(daos_handle_t oh, daos_handle_t th, uint64_t flags, void *ev)
{
	(void)flags;
	MOCK_ENTER("daos_obj_punch");
	if (ev)
		return -DER_INVAL;
	LOCKED(punch_locked(oh, th, NULL, 0, NULL, NULL, 0));
}

int
daos_obj_punch_dkeys(daos_handle_t oh, daos_handle_t th, uint64_t flags, unsigned nr,
		     d_iov_t *dkeys, void *ev)
{
	(void)flags;
	MOCK_ENTER("daos_obj_punch_dkeys");
	if (ev || !nr || !dkeys)
		return -DER_INVAL;
	LOCKED(punch_locked(oh, th, dkeys, nr, NULL, NULL, 0));
}

int
daos_obj_punch_akeys(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey,
		     unsigned nr, d_iov_t *akeys, void *ev)
{
	(void)flags;
	MOCK_ENTER("daos_obj_punch_akeys");
	if (ev || !nr || !akeys || !dkey)
		return -DER_INVAL;
	LOCKED(punch_locked(oh, th, NULL, 0, dkey, akeys, nr));
}

static int
cmp_key(const void *x, const void *y)
{
	const struct rec *a = *(struct rec *const *)x, *b = *(struct rec *const *)y;
	size_t            n = a->dlen < b->dlen ? a->dlen : b->dlen;
	int               c = memcmp(a->dkey, b->dkey, n);

	return c ? c : (int)a->dlen - (int)b->dlen;
}

/* List distinct dkeys (dkey == NULL) or the akeys of a dkey (stored in the
 * dkey field of a temporary copy for sorting). */
static int
list_locked(daos_handle_t oh, daos_handle_t th, d_iov_t *dkey, uint32_t *nr, daos_key_desc_t *kds,
	    d_sg_list_t *sgl, daos_anchor_t *anchor)
{
	struct objh   *o;
	const uint8_t *d = NULL;
	size_t         dl = 0;
	int            rc = data_check(oh, th, 0, &o);

	if (rc)
		return rc;
	if (!nr || !kds || !sgl || !anchor || sgl->sg_nr != 1)
		return -DER_INVAL;
	if (dkey && (rc = key_of(dkey, dkey_int(o->hi), &d, &dl)))
		return rc;
	size_t cap = sgl->sg_iovs[0].iov_buf_len;
	if (key2big_times > 0) {
		key2big_times--;
		kds[0].kd_key_len = key2big_report < 0 ? cap : (uint64_t)key2big_report;
		*nr               = 0;
		return -DER_KEY2BIG;
	}
	static struct rec  tmp[MAX_RECS];
	static struct rec *ptrs[MAX_RECS];
	size_t             n = 0;
	for (int i = 0; i < MAX_RECS; i++) {
		struct rec *r = &recs[i];
		if (!r->used || r->dead || r->cont != o->cont || r->lo != o->lo || r->hi != o->hi)
			continue;
		if (r->tx && r->tx != th.cookie)
			continue;
		if (d && (r->dlen != dl || memcmp(r->dkey, d, dl)))
			continue;
		memset(&tmp[n], 0, sizeof(tmp[n]));
		if (d) {
			memcpy(tmp[n].dkey, r->akey, r->alen);
			tmp[n].dlen = r->alen;
		} else {
			memcpy(tmp[n].dkey, r->dkey, r->dlen);
			tmp[n].dlen = r->dlen;
		}
		ptrs[n] = &tmp[n];
		n++;
	}
	qsort(ptrs, n, sizeof(ptrs[0]), cmp_key);
	size_t u = 0;
	for (size_t i = 0; i < n; i++)
		if (u == 0 || cmp_key(&ptrs[u - 1], &ptrs[i]) != 0)
			ptrs[u++] = ptrs[i];
	size_t start = anchor->da_sub_anchors, end = start, off = 0, got = 0;
	while (end < u && got < *nr) {
		struct rec *k = ptrs[end];
		if (off + k->dlen > cap) {
			if (got == 0) {
				kds[0].kd_key_len = k->dlen;
				*nr               = 0;
				return -DER_KEY2BIG;
			}
			break;
		}
		memcpy((uint8_t *)sgl->sg_iovs[0].iov_buf + off, k->dkey, k->dlen);
		kds[got++].kd_key_len = k->dlen;
		off += k->dlen;
		end++;
	}
	anchor->da_sub_anchors = end;
	if (end >= u)
		anchor->da_type = 3;
	*nr = (uint32_t)got;
	return 0;
}

int
daos_obj_list_dkey(daos_handle_t oh, daos_handle_t th, uint32_t *nr, daos_key_desc_t *kds,
		   d_sg_list_t *sgl, daos_anchor_t *anchor, void *ev)
{
	MOCK_ENTER("daos_obj_list_dkey");
	if (ev)
		return -DER_INVAL;
	LOCKED(list_locked(oh, th, NULL, nr, kds, sgl, anchor));
}

int
daos_obj_list_akey(daos_handle_t oh, daos_handle_t th, d_iov_t *dkey, uint32_t *nr,
		   daos_key_desc_t *kds, d_sg_list_t *sgl, daos_anchor_t *anchor, void *ev)
{
	MOCK_ENTER("daos_obj_list_akey");
	if (ev || !dkey)
		return -DER_INVAL;
	LOCKED(list_locked(oh, th, dkey, nr, kds, sgl, anchor));
}

static int
query_locked(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey, d_iov_t *akey,
	     daos_recx_t *recx)
{
	struct objh *o;
	int          rc = data_check(oh, th, 0, &o), found = 0;
	uint64_t     best = 0, dk = 0, ak = 0;

	if (rc)
		return rc;
	if (!dkey || !dkey_int(o->hi) || dkey->iov_len != 8)
		return -DER_INVAL;
	int max = (flags & GET_MAX) != 0;
	dk      = *(uint64_t *)dkey->iov_buf;
	if (flags & GET_DKEY) {
		for (int i = 0; i < MAX_RECS; i++) {
			struct rec *r = &recs[i];
			if (!r->used || r->dead || r->tx || r->cont != o->cont || r->lo != o->lo ||
			    r->hi != o->hi)
				continue;
			uint64_t v;
			memcpy(&v, r->dkey, 8);
			if (!found || (max ? v > best : v < best))
				best = v;
			found = 1;
		}
		if (!found)
			return -DER_NONEXIST;
		dk = best;
	}
	*(uint64_t *)dkey->iov_buf = dk;
	if (!akey)
		return 0;
	ak    = *(uint64_t *)akey->iov_buf;
	found = 0;
	for (int i = 0; i < MAX_RECS; i++) {
		struct rec *r = &recs[i];
		uint64_t    v;
		if (!r->used || r->dead || r->tx || r->cont != o->cont || r->lo != o->lo ||
		    r->hi != o->hi || r->dlen != 8 || memcmp(r->dkey, &dk, 8))
			continue;
		memcpy(&v, r->akey, 8);
		if (flags & GET_AKEY) {
			if (!found || (max ? v > best : v < best))
				best = v;
			found = 1;
		} else if (v == ak) {
			found = 1;
		}
	}
	if (!found)
		return -DER_NONEXIST;
	if (flags & GET_AKEY)
		ak = best;
	*(uint64_t *)akey->iov_buf = ak;
	if ((flags & GET_RECX) && recx) {
		found = 0;
		for (int i = 0; i < MAX_RECS; i++) {
			struct rec *r = &recs[i];
			if (!r->used || r->dead || r->tx || r->single || r->cont != o->cont ||
			    r->lo != o->lo || r->hi != o->hi || memcmp(r->dkey, &dk, 8) ||
			    memcmp(r->akey, &ak, 8))
				continue;
			if (!found || (max ? r->idx > best : r->idx < best))
				best = r->idx;
			found = 1;
		}
		if (!found)
			return -DER_NONEXIST;
		recx->rx_idx = best;
		recx->rx_nr  = 1;
	}
	return 0;
}

int
daos_obj_query_key(daos_handle_t oh, daos_handle_t th, uint64_t flags, d_iov_t *dkey,
		   d_iov_t *akey, daos_recx_t *recx, void *ev)
{
	MOCK_ENTER("daos_obj_query_key");
	if (ev)
		return -DER_INVAL;
	LOCKED(query_locked(oh, th, flags, dkey, akey, recx));
}

#ifndef NO_KV
int
daos_kv_open(daos_handle_t coh, daos_obj_id_t oid, unsigned mode, daos_handle_t *oh, void *ev)
{
	MOCK_ENTER("daos_kv_open");
	if (ev || !oh || otype(oid.hi) < 8 || otype(oid.hi) > 10)
		return -DER_INVAL;
	LOCKED(obj_open_kind(coh, oid, mode, oh, K_KV, 0, 0));
}

int
daos_kv_close(daos_handle_t oh, void *ev)
{
	MOCK_ENTER("daos_kv_close");
	if (ev)
		return -DER_INVAL;
	LOCKED(obj_close_kind(oh, K_KV));
}

int
daos_kv_destroy(daos_handle_t oh, daos_handle_t th, void *ev)
{
	MOCK_ENTER("daos_kv_destroy");
	if (ev)
		return -DER_INVAL;
	LOCKED(punch_locked(oh, th, NULL, 0, NULL, NULL, 0));
}

static int
kv_put_locked(daos_handle_t oh, daos_handle_t th, uint64_t flags, const char *key, uint64_t size,
	      const void *buf)
{
	struct objh *o;
	int          rc = data_check(oh, th, 1, &o);

	if (rc)
		return rc;
	if (!key || !key[0] || strlen(key) > MAX_KEY)
		return -DER_INVAL;
	if ((flags & COND_DKEY_INS) &&
	    r_find(o->cont, o->lo, o->hi, (const uint8_t *)key, strlen(key), (const uint8_t *)"0", 1,
		   1, 0, th.cookie))
		return -DER_EXIST;
	r_put(o->cont, o->lo, o->hi, (const uint8_t *)key, strlen(key), (const uint8_t *)"0", 1, 1, 0,
	      0, buf, size, th.cookie, 0);
	return 0;
}

int
daos_kv_put(daos_handle_t oh, daos_handle_t th, uint64_t flags, const char *key, uint64_t size,
	    const void *buf, void *ev)
{
	MOCK_ENTER("daos_kv_put");
	if (ev)
		return -DER_INVAL;
	LOCKED(kv_put_locked(oh, th, flags, key, size, buf));
}

static int
kv_get_locked(daos_handle_t oh, daos_handle_t th, const char *key, uint64_t *size, void *buf)
{
	struct objh *o;
	int          rc = data_check(oh, th, 0, &o);

	if (rc)
		return rc;
	if (!key || !size)
		return -DER_INVAL;
	struct rec *r = r_find(o->cont, o->lo, o->hi, (const uint8_t *)key, strlen(key),
			       (const uint8_t *)"0", 1, 1, 0, th.cookie);
	uint64_t cap = *size;
	*size        = r ? r->vlen : 0;
	if (!buf || cap == 0 || !r)
		return 0;
	if (r->vlen > cap)
		return -DER_REC2BIG;
	memcpy(buf, r->val, r->vlen);
	return 0;
}

int
daos_kv_get(daos_handle_t oh, daos_handle_t th, uint64_t flags, const char *key, uint64_t *size,
	    void *buf, void *ev)
{
	(void)flags;
	MOCK_ENTER("daos_kv_get");
	if (ev)
		return -DER_INVAL;
	LOCKED(kv_get_locked(oh, th, key, size, buf));
}

static int
kv_remove_locked(daos_handle_t oh, daos_handle_t th, const char *key)
{
	struct objh *o;
	int          rc = data_check(oh, th, 1, &o);

	if (rc)
		return rc;
	if (!key)
		return -DER_INVAL;
	r_punch(o->cont, o->lo, o->hi, (const uint8_t *)key, strlen(key), NULL, 0, th.cookie);
	return 0;
}

int
daos_kv_remove(daos_handle_t oh, daos_handle_t th, uint64_t flags, const char *key, void *ev)
{
	(void)flags;
	MOCK_ENTER("daos_kv_remove");
	if (ev)
		return -DER_INVAL;
	LOCKED(kv_remove_locked(oh, th, key));
}

int
daos_kv_list(daos_handle_t oh, daos_handle_t th, uint32_t *nr, daos_key_desc_t *kds,
	     d_sg_list_t *sgl, daos_anchor_t *anchor, void *ev)
{
	MOCK_ENTER("daos_kv_list");
	if (ev)
		return -DER_INVAL;
	LOCKED(list_locked(oh, th, NULL, nr, kds, sgl, anchor));
}
#endif /* NO_KV */

#ifndef NO_ARRAY
static int
array_create_locked(daos_handle_t coh, daos_obj_id_t oid, uint64_t cell, uint64_t chunk,
		    daos_handle_t *oh)
{
	struct handle *c = h_find(coh.cookie, H_CONT);

	if (!c)
		return -DER_NO_HDL;
	if (otype(oid.hi) != 11 || !cell || !chunk)
		return -DER_INVAL;
	struct arrmeta *m = a_find(c->cont, oid.lo, oid.hi, 1);
	if (m->cell)
		return -DER_EXIST;
	m->cell  = cell;
	m->chunk = chunk;
	return obj_open_kind(coh, oid, OO_RW, oh, K_ARRAY, cell, chunk);
}

int
daos_array_create(daos_handle_t coh, daos_obj_id_t oid, daos_handle_t th, uint64_t cell,
		  uint64_t chunk, daos_handle_t *oh, void *ev)
{
	(void)th;
	MOCK_ENTER("daos_array_create");
	if (ev || !oh)
		return -DER_INVAL;
	LOCKED(array_create_locked(coh, oid, cell, chunk, oh));
}

static int
array_open_locked(daos_handle_t coh, daos_obj_id_t oid, unsigned mode, uint64_t *cell,
		  uint64_t *chunk, daos_handle_t *oh)
{
	struct handle *c = h_find(coh.cookie, H_CONT);

	if (!c)
		return -DER_NO_HDL;
	if (otype(oid.hi) != 11)
		return -DER_INVAL;
	struct arrmeta *m = a_find(c->cont, oid.lo, oid.hi, 0);
	if (!m || !m->cell)
		return -DER_NONEXIST;
	*cell  = m->cell;
	*chunk = m->chunk;
	return obj_open_kind(coh, oid, mode, oh, K_ARRAY, m->cell, m->chunk);
}

int
daos_array_open(daos_handle_t coh, daos_obj_id_t oid, daos_handle_t th, unsigned mode,
		uint64_t *cell, uint64_t *chunk, daos_handle_t *oh, void *ev)
{
	(void)th;
	MOCK_ENTER("daos_array_open");
	if (ev || !oh || !cell || !chunk)
		return -DER_INVAL;
	LOCKED(array_open_locked(coh, oid, mode, cell, chunk, oh));
}

int
daos_array_open_with_attr(daos_handle_t coh, daos_obj_id_t oid, daos_handle_t th, unsigned mode,
			  uint64_t cell, uint64_t chunk, daos_handle_t *oh, void *ev)
{
	(void)th;
	MOCK_ENTER("daos_array_open_with_attr");
	if (ev || !oh || !cell || (otype(oid.hi) != 12 && otype(oid.hi) != 13))
		return -DER_INVAL;
	LOCKED(obj_open_kind(coh, oid, mode, oh, K_ARRAY, cell, chunk));
}

int
daos_array_close(daos_handle_t oh, void *ev)
{
	MOCK_ENTER("daos_array_close");
	if (ev)
		return -DER_INVAL;
	LOCKED(obj_close_kind(oh, K_ARRAY));
}

static uint64_t
array_size_of(struct objh *o)
{
	struct arrmeta *m = a_find(o->cont, o->lo, o->hi, 1);
	return m->size;
}

static int
array_io_locked(daos_handle_t oh, daos_handle_t th, daos_array_iod_t *iod, d_sg_list_t *sgl,
		int write)
{
	struct objh *o;
	int          rc = data_check(oh, th, write, &o);

	if (rc)
		return rc;
	if (o->kind != K_ARRAY || !iod || !sgl || sgl->sg_nr != 1)
		return -DER_INVAL;
	if (iod->arr_nr == 0) {
		fprintf(stderr, "array call with arr_nr == 0\n");
		abort();
	}
	int             byte = otype(o->hi) == 13;
	struct arrmeta *m    = a_find(o->cont, o->lo, o->hi, 1);
	uint8_t        *buf  = sgl->sg_iovs[0].iov_buf;
	uint64_t        off = 0, nread = 0, nshort = 0, size = m->size;
	for (uint64_t k = 0; k < iod->arr_nr; k++)
		for (uint64_t j = 0; j < iod->arr_rgs[k].rg_len; j++) {
			uint64_t idx = iod->arr_rgs[k].rg_idx + j;
			if ((off + 1) * o->cell > sgl->sg_iovs[0].iov_buf_len)
				return -DER_INVAL;
			if (write) {
				r_put(o->cont, o->lo, o->hi, (const uint8_t *)"__array", 7,
				      (const uint8_t *)"0", 1, 0, idx, o->cell, buf + off * o->cell,
				      o->cell, th.cookie, 0);
				if (idx + 1 > m->size)
					m->size = idx + 1;
				m->epoch = ++epoch_now;
			} else {
				struct rec *r = r_find(o->cont, o->lo, o->hi, (const uint8_t *)"__array",
						       7, (const uint8_t *)"0", 1, 0, idx, th.cookie);
				if (r)
					memcpy(buf + off * o->cell, r->val, o->cell);
				else if (byte && idx < size)
					memset(buf + off * o->cell, 0, o->cell);
				if (idx < size)
					nread++;
				else
					nshort++;
			}
			off++;
		}
	if (!write) {
		iod->arr_nr_read       = byte ? nread : 0xdeadbeefdeadbeefull;
		iod->arr_nr_short_read = byte ? nshort : 0xdeadbeefdeadbeefull;
	}
	return 0;
}

int
daos_array_read(daos_handle_t oh, daos_handle_t th, daos_array_iod_t *iod, d_sg_list_t *sgl,
		void *ev)
{
	MOCK_ENTER("daos_array_read");
	if (ev)
		return -DER_INVAL;
	LOCKED(array_io_locked(oh, th, iod, sgl, 0));
}

int
daos_array_write(daos_handle_t oh, daos_handle_t th, daos_array_iod_t *iod, d_sg_list_t *sgl,
		 void *ev)
{
	MOCK_ENTER("daos_array_write");
	if (ev)
		return -DER_INVAL;
	LOCKED(array_io_locked(oh, th, iod, sgl, 1));
}

static int
array_size_locked(daos_handle_t oh, daos_handle_t th, uint64_t *get, uint64_t set, int setting,
		  daos_array_stbuf_t *st)
{
	struct objh *o;
	int          rc = data_check(oh, th, setting, &o);

	if (rc)
		return rc;
	if (o->kind != K_ARRAY)
		return -DER_INVAL;
	struct arrmeta *m = a_find(o->cont, o->lo, o->hi, 1);
	if (setting) {
		for (int i = 0; i < MAX_RECS; i++)
			if (recs[i].used && recs[i].cont == o->cont && recs[i].lo == o->lo &&
			    recs[i].hi == o->hi && !recs[i].single && recs[i].idx >= set)
				r_drop(&recs[i]);
		m->size  = set;
		m->epoch = ++epoch_now;
	}
	if (get)
		*get = array_size_of(o);
	if (st) {
		st->st_size      = m->size;
		st->st_max_epoch = m->epoch;
	}
	return 0;
}

int
daos_array_get_size(daos_handle_t oh, daos_handle_t th, uint64_t *size, void *ev)
{
	MOCK_ENTER("daos_array_get_size");
	if (ev || !size)
		return -DER_INVAL;
	LOCKED(array_size_locked(oh, th, size, 0, 0, NULL));
}

int
daos_array_set_size(daos_handle_t oh, daos_handle_t th, uint64_t size, void *ev)
{
	MOCK_ENTER("daos_array_set_size");
	if (ev)
		return -DER_INVAL;
	LOCKED(array_size_locked(oh, th, NULL, size, 1, NULL));
}

int
daos_array_stat(daos_handle_t oh, daos_handle_t th, daos_array_stbuf_t *st, void *ev)
{
	MOCK_ENTER("daos_array_stat");
	if (ev || !st)
		return -DER_INVAL;
	LOCKED(array_size_locked(oh, th, NULL, 0, 0, st));
}

static int
array_punch_locked(daos_handle_t oh, daos_handle_t th, daos_array_iod_t *iod)
{
	struct objh *o;
	int          rc = data_check(oh, th, 1, &o);

	if (rc)
		return rc;
	if (!iod || iod->arr_nr == 0) {
		fprintf(stderr, "array punch with arr_nr == 0\n");
		abort();
	}
	for (uint64_t k = 0; k < iod->arr_nr; k++)
		for (uint64_t j = 0; j < iod->arr_rgs[k].rg_len; j++)
			for (int i = 0; i < MAX_RECS; i++)
				if (recs[i].used && recs[i].cont == o->cont && recs[i].lo == o->lo &&
				    recs[i].hi == o->hi && !recs[i].single &&
				    recs[i].idx == iod->arr_rgs[k].rg_idx + j)
					r_drop(&recs[i]);
	return 0;
}

int
daos_array_punch(daos_handle_t oh, daos_handle_t th, daos_array_iod_t *iod, void *ev)
{
	MOCK_ENTER("daos_array_punch");
	if (ev)
		return -DER_INVAL;
	LOCKED(array_punch_locked(oh, th, iod));
}

static int
array_destroy_locked(daos_handle_t oh, daos_handle_t th)
{
	struct objh *o;
	int          rc = data_check(oh, th, 1, &o);

	if (rc)
		return rc;
	r_punch(o->cont, o->lo, o->hi, NULL, 0, NULL, 0, 0);
	struct arrmeta *m = a_find(o->cont, o->lo, o->hi, 0);
	if (m)
		m->used = 0;
	return 0;
}

int
daos_array_destroy(daos_handle_t oh, daos_handle_t th, void *ev)
{
	MOCK_ENTER("daos_array_destroy");
	if (ev)
		return -DER_INVAL;
	LOCKED(array_destroy_locked(oh, th));
}
#endif /* NO_ARRAY */

static int
tx_open_locked(daos_handle_t coh, uint64_t flags, uint64_t snap, daos_handle_t *th)
{
	struct handle *c = h_find(coh.cookie, H_CONT);

	if (!c)
		return -DER_NO_HDL;
	for (int i = 0; i < MAX_TXS; i++)
		if (!txs[i].used) {
			memset(&txs[i], 0, sizeof(txs[i]));
			txs[i].used   = 1;
			txs[i].cookie = TX_BASE + next_cookie++;
			txs[i].cont   = c->cont;
			txs[i].ro     = (flags & TF_RDONLY) || snap;
			txs[i].snap   = snap;
			th->cookie    = txs[i].cookie;
			return 0;
		}
	abort();
}

int
daos_tx_open(daos_handle_t coh, daos_handle_t *th, uint64_t flags, void *ev)
{
	MOCK_ENTER("daos_tx_open");
	if (flags & TF_ZERO_COPY) {
		fprintf(stderr, "daos_tx_open: DAOS_TF_ZERO_COPY must never be passed\n");
		abort();
	}
	if (ev || !th)
		return -DER_INVAL;
	LOCKED(tx_open_locked(coh, flags, 0, th));
}

int
daos_tx_open_snap(daos_handle_t coh, uint64_t epoch, daos_handle_t *th, void *ev)
{
	MOCK_ENTER("daos_tx_open_snap");
	if (ev || !th || !epoch)
		return -DER_INVAL;
	LOCKED(tx_open_locked(coh, 0, epoch, th));
}

/* op: 0 commit, 1 abort, 2 restart, 3 close */
static int
tx_end_locked(daos_handle_t th, int op)
{
	struct txh *t = t_find(th.cookie);

	if (!t)
		return -DER_NO_HDL;
	if (op == 0 && t->owner_set && !pthread_equal(t->owner, pthread_self())) {
		cross_thread++;
		return -DER_INVAL;
	}
	/* State rules of dc_tx_commit, dc_tx_abort and dc_tx_restart_begin. */
	if (op == 0 && t->status == TXS_COMMITTED)
		return -DER_ALREADY;
	if (op == 1 && t->status == TXS_ABORTED)
		return -DER_ALREADY;
	if ((op == 0 || op == 1) && t->status != TXS_OPEN)
		return -DER_NO_PERM;
	if (op == 2 && t->status != TXS_FAILED)
		return -DER_NO_PERM;
	for (int i = 0; i < MAX_RECS; i++) {
		struct rec *r = &recs[i];
		if (!r->used || r->tx != th.cookie)
			continue;
		if (op == 0) {
			struct rec *c =
			    r_find(r->cont, r->lo, r->hi, r->dkey, r->dlen, r->akey, r->alen, r->single,
				   r->idx, 0);
			if (c && c != r)
				r_drop(c);
			if (r->dead)
				r_drop(r);
			else
				r->tx = 0;
		} else {
			r_drop(r);
		}
	}
	if (op == 0)
		t->status = TXS_COMMITTED;
	else if (op == 1)
		t->status = TXS_ABORTED;
	else if (op == 2)
		t->status = TXS_OPEN;
	else
		t->used = 0;
	return 0;
}

/* An injected commit result: the state change libdaos makes for it. */
static void
tx_commit_failed(daos_handle_t th, int rc)
{
	pthread_mutex_lock(&lock);
	struct txh *t = t_find(th.cookie);
	if (t && rc != 0 && t->status == TXS_OPEN) {
		for (int i = 0; i < MAX_RECS; i++)
			if (recs[i].used && recs[i].tx == th.cookie)
				r_drop(&recs[i]);
		t->status = rc == -DER_TX_RESTART ? TXS_FAILED : TXS_ABORTED;
	}
	pthread_mutex_unlock(&lock);
}

int
daos_tx_commit(daos_handle_t th, void *ev)
{
	int rc;

	if (mock_enter("daos_tx_commit", &rc)) {
		tx_commit_failed(th, rc);
		return rc;
	}
	if ((rc = event_der()))
		return rc;
	if (ev)
		return -DER_INVAL;
	LOCKED(tx_end_locked(th, 0));
}

int
daos_tx_abort(daos_handle_t th, void *ev)
{
	MOCK_ENTER("daos_tx_abort");
	if (ev)
		return -DER_INVAL;
	LOCKED(tx_end_locked(th, 1));
}

int
daos_tx_restart(daos_handle_t th, void *ev)
{
	MOCK_ENTER("daos_tx_restart");
	if (ev)
		return -DER_INVAL;
	LOCKED(tx_end_locked(th, 2));
}

int
daos_tx_close(daos_handle_t th, void *ev)
{
	MOCK_ENTER("daos_tx_close");
	if (ev)
		return -DER_INVAL;
	LOCKED(tx_end_locked(th, 3));
}

int
daos_tx_hdl2epoch(daos_handle_t th, uint64_t *epoch)
{
	int rc;

	if (mock_enter("daos_tx_hdl2epoch", &rc))
		return rc;
	pthread_mutex_lock(&lock);
	struct txh *t = t_find(th.cookie);
	if (t && epoch)
		*epoch = t->snap ? t->snap : ++epoch_now;
	pthread_mutex_unlock(&lock);
	return t ? 0 : -DER_NO_HDL;
}
