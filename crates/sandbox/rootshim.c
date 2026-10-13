/* rootshim: LD_PRELOAD helper for sandbox.sh --root.
 *
 * Inside the sandbox "root" is just a uid: no capabilities, and the
 * fuse-overlayfs daemon serving the layered system is a plain user process
 * too. Neither can open a file or directory whose mode denies its owner,
 * so an installer that creates a directory with mode 000 and chmods it
 * later (dpkg does this for every directory) breaks the layer: the daemon
 * has to open the new directory to finish the mkdir, and can't.
 *
 * This shim keeps the owner's bits in every mode an installer sets:
 * rwx for directories, rw for files. Nothing else changes; the installer's
 * own chmod to the final mode still happens (with the same bits kept).
 *
 * It also makes chown to a uid or gid the sandbox doesn't have succeed.
 * Only your own uid is mapped into the sandbox (as root), so the kernel
 * rejects any other id with EINVAL — and tar, dpkg-deb, cp -a and
 * install all restore ownership when run as root. The file simply stays
 * yours, which is the only ownership it could have here; the plain view
 * shows installed files as root's anyway.
 *
 *   cc -shared -fPIC -O2 -o rootshim.so rootshim.c
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <sys/stat.h>
#include <sys/types.h>

#define DIR_BITS  (S_IRWXU)            /* 0700 */
#define FILE_BITS (S_IRUSR | S_IWUSR)  /* 0600 */

static void *real(const char *name) { return dlsym(RTLD_NEXT, name); }

/* --- creation --------------------------------------------------------- */

int mkdir(const char *path, mode_t mode)
{
	static int (*next)(const char *, mode_t);
	if (!next) next = real("mkdir");
	return next(path, mode | DIR_BITS);
}

int mkdirat(int dirfd, const char *path, mode_t mode)
{
	static int (*next)(int, const char *, mode_t);
	if (!next) next = real("mkdirat");
	return next(dirfd, path, mode | DIR_BITS);
}

int creat(const char *path, mode_t mode)
{
	static int (*next)(const char *, mode_t);
	if (!next) next = real("creat");
	return next(path, mode | FILE_BITS);
}

/* open()'s mode is only meaningful (and only passed) with O_CREAT/O_TMPFILE */
static int wants_mode(int flags) { return (flags & (O_CREAT | O_TMPFILE)) != 0; }

int open(const char *path, int flags, ...)
{
	static int (*next)(const char *, int, ...);
	mode_t mode = 0;
	if (!next) next = real("open");
	if (wants_mode(flags)) {
		va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
		return next(path, flags, mode | FILE_BITS);
	}
	return next(path, flags);
}

int openat(int dirfd, const char *path, int flags, ...)
{
	static int (*next)(int, const char *, int, ...);
	mode_t mode = 0;
	if (!next) next = real("openat");
	if (wants_mode(flags)) {
		va_list ap; va_start(ap, flags); mode = va_arg(ap, mode_t); va_end(ap);
		return next(dirfd, path, flags, mode | FILE_BITS);
	}
	return next(dirfd, path, flags);
}

/* glibc exports the LFS names as separate symbols; same bodies */
int open64(const char *path, int flags, ...) __attribute__((alias("open")));
int openat64(int dirfd, const char *path, int flags, ...) __attribute__((alias("openat")));
int creat64(const char *path, mode_t mode) __attribute__((alias("creat")));

/* --- permission changes ----------------------------------------------- */

static mode_t keep_bits(mode_t mode, int is_dir) { return mode | (is_dir ? DIR_BITS : FILE_BITS); }

int chmod(const char *path, mode_t mode)
{
	static int (*next)(const char *, mode_t);
	struct stat st;
	if (!next) next = real("chmod");
	return next(path, keep_bits(mode, stat(path, &st) == 0 && S_ISDIR(st.st_mode)));
}

int fchmod(int fd, mode_t mode)
{
	static int (*next)(int, mode_t);
	struct stat st;
	if (!next) next = real("fchmod");
	return next(fd, keep_bits(mode, fstat(fd, &st) == 0 && S_ISDIR(st.st_mode)));
}

int fchmodat(int dirfd, const char *path, mode_t mode, int flags)
{
	static int (*next)(int, const char *, mode_t, int);
	struct stat st;
	if (!next) next = real("fchmodat");
	return next(dirfd, path, keep_bits(mode, fstatat(dirfd, path, &st, flags) == 0 && S_ISDIR(st.st_mode)), flags);
}

/* --- ownership changes ------------------------------------------------ */

/* EINVAL from chown means the id isn't mapped in our user namespace */
static int unmapped(int rc) { return rc < 0 && errno == EINVAL ? 0 : rc; }

int chown(const char *path, uid_t uid, gid_t gid)
{
	static int (*next)(const char *, uid_t, gid_t);
	if (!next) next = real("chown");
	return unmapped(next(path, uid, gid));
}

int lchown(const char *path, uid_t uid, gid_t gid)
{
	static int (*next)(const char *, uid_t, gid_t);
	if (!next) next = real("lchown");
	return unmapped(next(path, uid, gid));
}

int fchown(int fd, uid_t uid, gid_t gid)
{
	static int (*next)(int, uid_t, gid_t);
	if (!next) next = real("fchown");
	return unmapped(next(fd, uid, gid));
}

int fchownat(int dirfd, const char *path, uid_t uid, gid_t gid, int flags)
{
	static int (*next)(int, const char *, uid_t, gid_t, int);
	if (!next) next = real("fchownat");
	return unmapped(next(dirfd, path, uid, gid, flags));
}
