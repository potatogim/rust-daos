// Copyright (c) 2026 Jihyeon Gim
// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions transcribed from DAOS headers, BSD-2-Clause-Patent, see LICENSE-THIRD-PARTY
//
// Stand-in for libdfs.so, used by tests/dlopen_mock.rs. Linked against
// libdaos_mock.so.2 (rpath $ORIGIN) like the real libdfs is linked against
// libdaos, so the crate's lookup of daos_* through the libdfs handle is
// exercised. Implements the dfs_* calls of the crate over an in-memory table.
// dfs_mount takes pool and container handles from libdaos_mock and holds the
// container (its daos_cont_close returns -DER_BUSY until dfs_umount); it
// fails with EINVAL on a container that is not a POSIX container.
//
// Build variants: -DOMIT_DFS_STAT leaves out a required symbol.
//
// Load pause: when RUST_DAOS_MOCK_LOAD_PAUSE names a directory, the library
// constructor creates "entered" in it and waits (up to 10 s) until "release"
// exists, so a test can fork while another thread is inside dlopen. Without
// the variable the constructor does nothing.
//
// Calls and error paths only; layouts are checked by the ABI probe.

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* Declarations as in gurt/types.h, daos_types.h and daos_fs.h. */
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
#define DAOS_ANCHOR_BUF_MAX 104
typedef struct {
	uint16_t da_type;
	uint16_t da_shard;
	uint32_t da_flags;
	uint64_t da_sub_anchors;
	uint8_t  da_buf[DAOS_ANCHOR_BUF_MAX];
} daos_anchor_t;
#define DAOS_ANCHOR_TYPE_EOF 3
typedef struct {
	uint64_t cookie;
} daos_handle_t;
typedef struct dfs     dfs_t;
typedef struct dfs_obj dfs_obj_t;
typedef int (*dfs_filler_cb_t)(dfs_t *dfs, dfs_obj_t *obj, const char name[], void *arg);

__attribute__((constructor)) static void
mock_load_pause(void)
{
	const char *dir = getenv("RUST_DAOS_MOCK_LOAD_PAUSE");
	char        path[4096];
	int         fd;

	if (dir == NULL || dir[0] == 0)
		return;
	snprintf(path, sizeof(path), "%s/entered", dir);
	fd = open(path, O_CREAT | O_WRONLY, 0600);
	if (fd >= 0)
		close(fd);
	snprintf(path, sizeof(path), "%s/release", dir);
	for (int i = 0; i < 1000 && access(path, F_OK) != 0; i++)
		usleep(10000);
}

/* From libdaos_mock.so.2. */
int mock_enter(const char *name, int *rc);
int mock_thread_event_check(void);
int mock_daos_initialized(void);
int mock_cont_use(daos_handle_t poh, daos_handle_t coh, int delta, int *posix, int *rw);
int mock_cont_create(daos_handle_t poh, const char *label, void *prop, unsigned char *uuid,
		     int posix);

/* dfs_attr_t (daos_fs.h); da_props points to a daos_prop_t. */
typedef struct {
	uint64_t da_id;
	uint64_t da_chunk_size;
	uint32_t da_oclass_id;
	void    *da_props;
	uint32_t da_mode;
	uint32_t da_dir_oclass_id;
	uint32_t da_file_oclass_id;
	char     da_hints[128];
} dfs_attr_t;

/* Map the DER codes mock_cont_create returns to errno, as libdfs does. */
static int
der2errno(int rc)
{
	switch (rc) {
	case 0:
		return 0;
	case -1001:
		return EPERM;
	case -1002:
	case -1003:
		return EINVAL;
	case -1004:
		return EEXIST;
	case -1005:
		return ENOENT;
	default:
		return EIO;
	}
}

struct dfs {
	int           ro;
	daos_handle_t poh;
	daos_handle_t coh;
};
struct dfs_obj {
	char     path[4096];
	uint64_t oid;
	mode_t   mode;
	int      flags;
};
struct entry {
	char    *path;
	uint64_t oid;
	mode_t   mode;
	char    *data;
	size_t   size;
};

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct entry   *ents;
static size_t          nents, cap;
static uint64_t        next_oid = 2;
static int             dfs_count;
static long            live_objects;

/* Objects returned by dfs_lookup/dfs_open and not yet released. */
long
mock_live_objects(void)
{
	long n;

	pthread_mutex_lock(&lock);
	n = live_objects;
	pthread_mutex_unlock(&lock);
	return n;
}

#define ENTER(name)                                                                                \
	do {                                                                                       \
		int _rc;                                                                           \
		if (mock_enter(name, &_rc))                                                        \
			return _rc;                                                                \
	} while (0)
#define EVENT()                                                                                    \
	do {                                                                                       \
		int _rc = mock_thread_event_check();                                               \
		if (_rc)                                                                           \
			return _rc;                                                                \
	} while (0)

static struct entry *
find(const char *path)
{
	for (size_t i = 0; i < nents; i++)
		if (strcmp(ents[i].path, path) == 0)
			return &ents[i];
	return NULL;
}

static struct entry *
add(const char *path, mode_t mode)
{
	if (nents == cap) {
		cap  = cap ? cap * 2 : 64;
		ents = realloc(ents, cap * sizeof(*ents));
	}
	struct entry *e = &ents[nents++];
	memset(e, 0, sizeof(*e));
	e->path = strdup(path);
	e->oid  = next_oid++;
	e->mode = mode;
	return e;
}

static void
del(struct entry *e)
{
	free(e->path);
	free(e->data);
	*e = ents[--nents];
}

static void
ensure_root(void)
{
	if (!find("/"))
		add("/", S_IFDIR | 0755);
}

static int
child(char *out, const char *dir, const char *name)
{
	size_t len;

	if (name == NULL || (len = strlen(name)) == 0 || len > 255 || strchr(name, '/'))
		return EINVAL;
	if (strcmp(dir, "/") == 0)
		snprintf(out, 4096, "/%s", name);
	else
		snprintf(out, 4096, "%s/%s", dir, name);
	return 0;
}

static int
parent_path(dfs_obj_t *parent, const char **dir)
{
	if (parent == NULL) {
		*dir = "/";
		return 0;
	}
	if (!S_ISDIR(parent->mode))
		return ENOTDIR;
	struct entry *e = find(parent->path);
	if (!e || e->oid != parent->oid)
		return ENOENT;
	*dir = parent->path;
	return 0;
}

static int
is_below(const char *path, const char *dir)
{
	size_t n = strlen(dir);

	if (strcmp(dir, "/") == 0)
		return strcmp(path, "/") != 0;
	return strncmp(path, dir, n) == 0 && path[n] == '/';
}

static int
is_child(const char *path, const char *dir)
{
	if (!is_below(path, dir))
		return 0;
	const char *rest = path + (strcmp(dir, "/") == 0 ? 1 : strlen(dir) + 1);
	return strchr(rest, '/') == NULL;
}

static void
fill(struct entry *e, struct stat *st)
{
	memset(st, 0, sizeof(*st));
	st->st_mode = e->mode;
	st->st_size = (off_t)e->size;
	st->st_uid  = geteuid();
	st->st_gid  = getegid();
}

int
dfs_init(void)
{
	ENTER("dfs_init");
	if (!mock_daos_initialized())
		return EIO;
	pthread_mutex_lock(&lock);
	dfs_count++;
	ensure_root();
	pthread_mutex_unlock(&lock);
	return 0;
}

int
dfs_fini(void)
{
	ENTER("dfs_fini");
	pthread_mutex_lock(&lock);
	dfs_count--;
	pthread_mutex_unlock(&lock);
	return 0;
}

int
dfs_mount(daos_handle_t poh, daos_handle_t coh, int flags, dfs_t **dfs)
{
	int posix = 0, rw = 0, rc;

	ENTER("dfs_mount");
	EVENT();
	if (dfs_count == 0)
		return EACCES;
	if (!dfs)
		return EINVAL;
	rc = mock_cont_use(poh, coh, 0, &posix, &rw);
	if (rc)
		return rc;
	if (!posix)
		return EINVAL;
	if ((flags & O_ACCMODE) == O_RDWR && !rw)
		return EPERM;
	rc = mock_cont_use(poh, coh, 1, NULL, NULL);
	if (rc)
		return rc;
	*dfs        = calloc(1, sizeof(**dfs));
	(*dfs)->ro  = (flags & O_ACCMODE) == O_RDONLY;
	(*dfs)->poh = poh;
	(*dfs)->coh = coh;
	return 0;
}

int
dfs_cont_create(daos_handle_t poh, unsigned char *uuid, dfs_attr_t *attr, daos_handle_t *coh,
		dfs_t **dfs)
{
	ENTER("dfs_cont_create");
	EVENT();
	if (!uuid || coh || dfs)
		return EINVAL;
	return der2errno(mock_cont_create(poh, NULL, attr ? attr->da_props : NULL, uuid, 1));
}

int
dfs_cont_create_with_label(daos_handle_t poh, const char *label, dfs_attr_t *attr,
			   unsigned char *uuid, daos_handle_t *coh, dfs_t **dfs)
{
	ENTER("dfs_cont_create_with_label");
	EVENT();
	if (!label || coh || dfs)
		return EINVAL;
	return der2errno(mock_cont_create(poh, label, attr ? attr->da_props : NULL, uuid, 1));
}

int
dfs_umount(dfs_t *dfs)
{
	ENTER("dfs_umount");
	EVENT();
	if (!dfs)
		return EINVAL;
	mock_cont_use(dfs->poh, dfs->coh, -1, NULL, NULL);
	free(dfs);
	return 0;
}

int
dfs_lookup(dfs_t *dfs, const char *path, int flags, dfs_obj_t **obj, mode_t *mode, struct stat *stbuf)
{
	ENTER("dfs_lookup");
	EVENT();
	if (!dfs || !path || !obj || path[0] != '/')
		return EINVAL;
	if (dfs->ro && (flags & O_ACCMODE) != O_RDONLY)
		return EPERM;
	char norm[4096];
	snprintf(norm, sizeof(norm), "%s", path);
	size_t n = strlen(norm);
	while (n > 1 && norm[n - 1] == '/')
		norm[--n] = 0;
	pthread_mutex_lock(&lock);
	struct entry *e = find(norm);
	if (!e) {
		pthread_mutex_unlock(&lock);
		return ENOENT;
	}
	dfs_obj_t *o = calloc(1, sizeof(*o));
	live_objects++;
	snprintf(o->path, sizeof(o->path), "%s", norm);
	o->oid   = e->oid;
	o->mode  = e->mode;
	o->flags = flags;
	if (mode)
		*mode = e->mode;
	if (stbuf)
		fill(e, stbuf);
	pthread_mutex_unlock(&lock);
	*obj = o;
	return 0;
}

int
dfs_open(dfs_t *dfs, dfs_obj_t *parent, const char *name, mode_t mode, int flags, uint32_t cid,
	 uint64_t chunk_size, const char *value, dfs_obj_t **obj)
{
	const char *dir;
	char        path[4096];
	int         rc;

	(void)cid;
	(void)chunk_size;
	(void)value;
	ENTER("dfs_open");
	EVENT();
	if (!dfs)
		return EINVAL;
	if (flags & O_APPEND)
		return ENOTSUP;
	if (dfs->ro && (flags & O_CREAT))
		return EPERM;
	if (!obj || !S_ISREG(mode))
		return EINVAL;
	pthread_mutex_lock(&lock);
	rc = parent_path(parent, &dir);
	if (rc == 0)
		rc = child(path, dir, name);
	struct entry *e = rc ? NULL : find(path);
	if (rc == 0 && e) {
		if ((flags & O_CREAT) && (flags & O_EXCL))
			rc = EEXIST;
		else if (!S_ISREG(e->mode))
			rc = EINVAL;
		else if (flags & O_TRUNC)
			e->size = 0;
	} else if (rc == 0) {
		if (flags & O_CREAT)
			e = add(path, mode);
		else
			rc = ENOENT;
	}
	if (rc == 0) {
		dfs_obj_t *o = calloc(1, sizeof(*o));
		live_objects++;
		snprintf(o->path, sizeof(o->path), "%s", path);
		o->oid   = e->oid;
		o->mode  = mode;
		o->flags = flags;
		*obj     = o;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
dfs_release(dfs_obj_t *obj)
{
	ENTER("dfs_release");
	EVENT();
	if (!obj)
		return EINVAL;
	pthread_mutex_lock(&lock);
	live_objects--;
	pthread_mutex_unlock(&lock);
	free(obj);
	return 0;
}

static struct entry *
obj_entry(dfs_obj_t *obj, int *rc)
{
	if (!obj) {
		*rc = EINVAL;
		return NULL;
	}
	struct entry *e = find(obj->path);
	if (!e || e->oid != obj->oid) {
		*rc = ENOENT;
		return NULL;
	}
	*rc = 0;
	return e;
}

int
dfs_read(dfs_t *dfs, dfs_obj_t *obj, d_sg_list_t *sgl, uint64_t off, uint64_t *read_size, void *ev)
{
	int rc;

	ENTER("dfs_read");
	EVENT();
	if (!dfs || !obj || !S_ISREG(obj->mode) || !sgl || !read_size || ev != NULL)
		return EINVAL;
	if ((obj->flags & O_ACCMODE) == O_WRONLY)
		return EPERM;
	if (sgl->sg_nr != 1 || !sgl->sg_iovs || sgl->sg_iovs[0].iov_len > sgl->sg_iovs[0].iov_buf_len)
		return EINVAL;
	pthread_mutex_lock(&lock);
	struct entry *e = obj_entry(obj, &rc);
	if (e) {
		size_t want = sgl->sg_iovs[0].iov_len;
		size_t n    = off >= e->size ? 0 : e->size - off;
		if (n > want)
			n = want;
		memcpy(sgl->sg_iovs[0].iov_buf, e->data + off, n);
		*read_size = n;
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
dfs_write(dfs_t *dfs, dfs_obj_t *obj, d_sg_list_t *sgl, uint64_t off, void *ev)
{
	int rc;

	ENTER("dfs_write");
	EVENT();
	if (!dfs || ev != NULL)
		return EINVAL;
	if (dfs->ro)
		return EPERM;
	if (!obj || !S_ISREG(obj->mode) || !sgl)
		return EINVAL;
	if ((obj->flags & O_ACCMODE) == O_RDONLY)
		return EPERM;
	if (sgl->sg_nr != 1 || !sgl->sg_iovs || sgl->sg_iovs[0].iov_len > sgl->sg_iovs[0].iov_buf_len)
		return EINVAL;
	pthread_mutex_lock(&lock);
	struct entry *e = obj_entry(obj, &rc);
	if (e) {
		size_t len = sgl->sg_iovs[0].iov_len;
		if (off + len > e->size) {
			e->data = realloc(e->data, off + len);
			memset(e->data + e->size, 0, off + len - e->size);
			e->size = off + len;
		}
		memcpy(e->data + off, sgl->sg_iovs[0].iov_buf, len);
	}
	pthread_mutex_unlock(&lock);
	return rc;
}

int
dfs_get_size(dfs_t *dfs, dfs_obj_t *obj, uint64_t *size)
{
	int rc;

	ENTER("dfs_get_size");
	EVENT();
	if (!dfs || !size)
		return EINVAL;
	pthread_mutex_lock(&lock);
	struct entry *e = obj_entry(obj, &rc);
	if (e)
		*size = e->size;
	pthread_mutex_unlock(&lock);
	return rc;
}

static int
cmp_str(const void *a, const void *b)
{
	return strcmp(*(char *const *)a, *(char *const *)b);
}

int
dfs_iterate(dfs_t *dfs, dfs_obj_t *obj, daos_anchor_t *anchor, uint32_t *nr, size_t size,
	    dfs_filler_cb_t op, void *arg)
{
	int rc;

	ENTER("dfs_iterate");
	EVENT();
	if (!dfs)
		return EINVAL;
	if (!obj || !S_ISDIR(obj->mode))
		return ENOTDIR;
	if (size == 0 || *nr == 0)
		return 0;
	if (!anchor || !op)
		return EINVAL;
	pthread_mutex_lock(&lock);
	if (!obj_entry(obj, &rc)) {
		pthread_mutex_unlock(&lock);
		return rc;
	}
	char **names = calloc(nents + 1, sizeof(char *));
	size_t count = 0;
	size_t skip  = strcmp(obj->path, "/") == 0 ? 1 : strlen(obj->path) + 1;
	for (size_t i = 0; i < nents; i++)
		if (is_child(ents[i].path, obj->path))
			names[count++] = strdup(ents[i].path + skip);
	pthread_mutex_unlock(&lock);
	qsort(names, count, sizeof(char *), cmp_str);

	size_t start = anchor->da_sub_anchors, end = start, bytes = 0;
	while (end < count && end - start < *nr && bytes + strlen(names[end]) + 1 <= size)
		bytes += strlen(names[end++]) + 1;
	anchor->da_sub_anchors = end;
	if (end >= count)
		anchor->da_type = DAOS_ANCHOR_TYPE_EOF;
	rc = 0;
	for (size_t i = start; i < end && rc == 0; i++) {
		char buf[256];
		snprintf(buf, sizeof(buf), "%s", names[i]);
		rc = op(dfs, obj, buf, arg);
	}
	if (rc == 0)
		*nr = (uint32_t)(end - start);
	for (size_t i = 0; i < count; i++)
		free(names[i]);
	free(names);
	return rc;
}

int
dfs_mkdir(dfs_t *dfs, dfs_obj_t *parent, const char *name, mode_t mode, uint32_t cid)
{
	const char *dir;
	char        path[4096];
	int         rc;

	(void)cid;
	ENTER("dfs_mkdir");
	EVENT();
	if (!dfs)
		return EINVAL;
	if (dfs->ro)
		return EPERM;
	pthread_mutex_lock(&lock);
	rc = parent_path(parent, &dir);
	if (rc == 0)
		rc = child(path, dir, name);
	if (rc == 0 && find(path))
		rc = EEXIST;
	if (rc == 0)
		add(path, S_IFDIR | mode);
	pthread_mutex_unlock(&lock);
	return rc;
}

int
dfs_remove(dfs_t *dfs, dfs_obj_t *parent, const char *name, bool force, void *oid)
{
	const char *dir;
	char        path[4096];
	int         rc;

	(void)oid;
	ENTER("dfs_remove");
	EVENT();
	if (!dfs)
		return EINVAL;
	if (dfs->ro)
		return EPERM;
	pthread_mutex_lock(&lock);
	rc = parent_path(parent, &dir);
	if (rc == 0)
		rc = child(path, dir, name);
	struct entry *e = rc ? NULL : find(path);
	if (rc == 0 && !e)
		rc = ENOENT;
	if (rc == 0 && S_ISDIR(e->mode)) {
		int has = 0;
		for (size_t i = 0; i < nents; i++)
			if (is_below(ents[i].path, path))
				has = 1;
		if (has && !force) {
			rc = ENOTEMPTY;
		} else {
			for (size_t i = 0; i < nents;)
				if (is_below(ents[i].path, path))
					del(&ents[i]);
				else
					i++;
		}
	}
	if (rc == 0)
		del(find(path));
	pthread_mutex_unlock(&lock);
	return rc;
}

#ifndef OMIT_DFS_STAT
int
dfs_stat(dfs_t *dfs, dfs_obj_t *parent, const char *name, struct stat *stbuf)
{
	const char *dir;
	char        path[4096];
	int         rc = 0;

	ENTER("dfs_stat");
	EVENT();
	if (!dfs || !stbuf)
		return EINVAL;
	pthread_mutex_lock(&lock);
	if (parent == NULL && name == NULL) {
		snprintf(path, sizeof(path), "/");
	} else {
		rc = parent_path(parent, &dir);
		if (rc == 0)
			rc = child(path, dir, name);
	}
	struct entry *e = rc ? NULL : find(path);
	if (rc == 0 && !e)
		rc = ENOENT;
	if (rc == 0)
		fill(e, stbuf);
	pthread_mutex_unlock(&lock);
	return rc;
}
#endif

int
dfs_ostat(dfs_t *dfs, dfs_obj_t *obj, struct stat *stbuf)
{
	int rc;

	ENTER("dfs_ostat");
	EVENT();
	if (!dfs || !stbuf)
		return EINVAL;
	pthread_mutex_lock(&lock);
	struct entry *e = obj_entry(obj, &rc);
	if (e)
		fill(e, stbuf);
	pthread_mutex_unlock(&lock);
	return rc;
}
