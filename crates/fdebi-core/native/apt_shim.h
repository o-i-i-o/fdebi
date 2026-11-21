/* fdebi libapt-pkg C ABI.
 *
 * The shim keeps every libapt-pkg object on the C++ side and exposes only
 * opaque handles and text records.  One FdebiApt owns an apt cache built
 * from the system configuration plus any "volatile" local .deb files, and
 * holds a global apt mutex for its whole lifetime (libapt-pkg uses process
 * wide globals such as _config/_error and is not safe for concurrent use).
 *
 * Text records use '\t' between fields and '\n' between records.  Debian
 * package names/versions/architectures cannot contain either byte, and
 * repository archive basenames are controlled by the archive, so the
 * encoding is unambiguous for everything crossing this boundary.
 */
#ifndef FDEBI_APT_SHIM_H
#define FDEBI_APT_SHIM_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct FdebiApt FdebiApt;

/* Fetch progress callback: current/total bytes.  Return non-zero to cancel. */
typedef int (*fdebi_fetch_cb)(void *user, unsigned long long current,
                              unsigned long long total);

FdebiApt *fdebi_apt_open(void);
void fdebi_apt_close(FdebiApt *ctx);

int fdebi_apt_set_option(FdebiApt *ctx, const char *key, const char *value);
int fdebi_apt_add_volatile(FdebiApt *ctx, const char *path);
int fdebi_apt_build(FdebiApt *ctx);

const char *fdebi_apt_native_arch(FdebiApt *ctx);
/* newline separated */
const char *fdebi_apt_foreign_arches(FdebiApt *ctx);

/* Empty string when not installed / no repository candidate. */
const char *fdebi_apt_installed_version(FdebiApt *ctx, const char *key);
const char *fdebi_apt_repo_version(FdebiApt *ctx, const char *key);

/* Installed version's Conffiles field: "path\tmd5\n" lines. */
const char *fdebi_apt_conffiles(FdebiApt *ctx, const char *key);

/* One package key per line: half-configured packages plus dependency-broken
 * installed packages. */
const char *fdebi_apt_broken(FdebiApt *ctx);

/* Solve installing the given local .deb.  Prior entries are
 * "key\tversion\tarch\tpath" strings describing earlier members of a batch
 * that were already accepted; they are marked for installation (and protected)
 * for this solve, so later batch members can depend on them.
 *
 * Returns:
 *   0  installable (results in fdebi_apt_result_*)
 *   1  dependency solving failed
 *   2  invalid package (missing Package/Version field)
 *   3  wrong architecture
 *   4  a later version is already installed
 *  -1  internal failure (details in fdebi_apt_last_error)
 */
int fdebi_apt_solve(FdebiApt *ctx, const char *name, const char *version,
                    const char *arch, const char *path,
                    const char *const *priors, size_t n_priors);

/* Results of the last successful fdebi_apt_solve. */
/* name\tkind\told_version\tnew_version\n */
const char *fdebi_apt_result_install(FdebiApt *ctx);
/* name\n */
const char *fdebi_apt_result_remove(FdebiApt *ctx);
/* name\n */
const char *fdebi_apt_result_unauth(FdebiApt *ctx);
/* name\tversion\tsize\ttrusted(0|1)\n */
const char *fdebi_apt_result_downloads(FdebiApt *ctx);

const char *fdebi_apt_last_error(FdebiApt *ctx);

/* Fetch one selected repository archive.  "name" is the apt package key
 * (FullName(true), e.g. "foo" or "foo:i386").  Returns a malloc'ed path that
 * must be released with fdebi_apt_free_string, or NULL on failure/cancel. */
char *fdebi_apt_fetch(FdebiApt *ctx, const char *name, const char *version,
                      fdebi_fetch_cb cb, void *user);
/* Non-zero when the last fdebi_apt_fetch returned NULL because the progress
 * callback asked for cancellation. */
int fdebi_apt_cancelled(FdebiApt *ctx);

/* Reverse dependency warning: package\trequires\n */
const char *fdebi_apt_reverse_deps(FdebiApt *ctx, const char *removed);

/* Mark auto packages and one manual package, then write extended_states. */
int fdebi_apt_mark(FdebiApt *ctx, const char *const *auto_names,
                   size_t n_auto, const char *manual);

void fdebi_apt_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* FDEBI_APT_SHIM_H */
