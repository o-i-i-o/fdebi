/* Non-Linux stub for the fdebi libapt-pkg shim.
 *
 * fdebi only supports Debian-derived systems at runtime.  The stub keeps the
 * crate buildable (and lets `cargo check` validate the Rust side) on Windows
 * and other hosts without libapt-pkg.  fdebi_apt_open() fails, so no caller
 * ever reaches the other functions.
 */
#include "apt_shim.h"

#include <stdlib.h>

static const char fdebi_stub_empty[] = "";

FdebiApt *fdebi_apt_open(void) { return NULL; }
void fdebi_apt_close(FdebiApt *ctx) { (void)ctx; }

int fdebi_apt_set_option(FdebiApt *ctx, const char *key, const char *value) {
   (void)ctx;
   (void)key;
   (void)value;
   return -1;
}

int fdebi_apt_add_volatile(FdebiApt *ctx, const char *path) {
   (void)ctx;
   (void)path;
   return -1;
}

int fdebi_apt_build(FdebiApt *ctx) {
   (void)ctx;
   return -1;
}

const char *fdebi_apt_native_arch(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

const char *fdebi_apt_foreign_arches(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

const char *fdebi_apt_installed_version(FdebiApt *ctx, const char *key) {
   (void)ctx;
   (void)key;
   return fdebi_stub_empty;
}

const char *fdebi_apt_repo_version(FdebiApt *ctx, const char *key) {
   (void)ctx;
   (void)key;
   return fdebi_stub_empty;
}

const char *fdebi_apt_conffiles(FdebiApt *ctx, const char *key) {
   (void)ctx;
   (void)key;
   return fdebi_stub_empty;
}

const char *fdebi_apt_broken(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

int fdebi_apt_solve(FdebiApt *ctx, const char *name, const char *version,
                    const char *arch, const char *path,
                    const char *const *priors, size_t n_priors) {
   (void)ctx;
   (void)name;
   (void)version;
   (void)arch;
   (void)path;
   (void)priors;
   (void)n_priors;
   return -1;
}

const char *fdebi_apt_result_install(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

const char *fdebi_apt_result_remove(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

const char *fdebi_apt_result_unauth(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

const char *fdebi_apt_result_downloads(FdebiApt *ctx) {
   (void)ctx;
   return fdebi_stub_empty;
}

const char *fdebi_apt_last_error(FdebiApt *ctx) {
   (void)ctx;
   return "libapt-pkg is only available on Linux";
}

char *fdebi_apt_fetch(FdebiApt *ctx, const char *name, const char *version,
                      fdebi_fetch_cb cb, void *user) {
   (void)ctx;
   (void)name;
   (void)version;
   (void)cb;
   (void)user;
   return NULL;
}

int fdebi_apt_cancelled(FdebiApt *ctx) {
   (void)ctx;
   return 0;
}

const char *fdebi_apt_reverse_deps(FdebiApt *ctx, const char *removed) {
   (void)ctx;
   (void)removed;
   return fdebi_stub_empty;
}

int fdebi_apt_mark(FdebiApt *ctx, const char *const *auto_names, size_t n_auto,
                   const char *manual) {
   (void)ctx;
   (void)auto_names;
   (void)n_auto;
   (void)manual;
   return -1;
}

void fdebi_apt_free_string(char *s) { free(s); }
