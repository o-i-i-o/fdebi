/* fdebi libapt-pkg shim implementation.
 *
 * All libapt-pkg access lives here.  The Rust side only sees text records and
 * opaque handles, so no apt type ever crosses the FFI boundary.
 */
#include "apt_shim.h"

#include <apt-pkg/acquire-item.h>
#include <apt-pkg/acquire.h>
#include <apt-pkg/algorithms.h>
#include <apt-pkg/aptconfiguration.h>
#include <apt-pkg/cachefile.h>
#include <apt-pkg/configuration.h>
#include <apt-pkg/depcache.h>
#include <apt-pkg/error.h>
#include <apt-pkg/indexfile.h>
#include <apt-pkg/init.h>
#include <apt-pkg/pkgcache.h>
#include <apt-pkg/pkgrecords.h>
#include <apt-pkg/pkgsystem.h>
#include <apt-pkg/policy.h>
#include <apt-pkg/sourcelist.h>

#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <mutex>
#include <new>
#include <set>
#include <sstream>
#include <string>
#include <unistd.h>
#include <utility>
#include <vector>

namespace {

std::mutex &apt_mutex() {
   static std::mutex mutex;
   return mutex;
}

struct FdebiApt {
   std::unique_lock<std::mutex> lock;
   std::unique_ptr<pkgCacheFile> cache;
   std::vector<std::pair<std::string, std::string>> options;
   std::vector<std::string> volatile_files;
   bool built = false;
   bool cancelled = false;
   std::string scratch;
   std::string last_error;
   std::string result_install;
   std::string result_remove;
   std::string result_unauth;
   std::string result_downloads;

   FdebiApt() : lock(apt_mutex()) {}
};

std::string take_errors() {
   std::string out;
   std::string message;
   while (_error->PopMessage(message)) {
      if (!out.empty())
         out.push_back('\n');
      out += message;
   }
   return out;
}

void fail(FdebiApt *ctx, std::string const &message) {
   if (ctx != nullptr) {
      ctx->last_error = message;
      if (ctx->last_error.empty())
         ctx->last_error = take_errors();
   }
   _error->Discard();
}

void fail_apt(FdebiApt *ctx, char const *fallback) {
   std::string errors = take_errors();
   if (ctx != nullptr)
      ctx->last_error = errors.empty() ? fallback : errors;
   _error->Discard();
}

void clear_results(FdebiApt *ctx) {
   ctx->result_install.clear();
   ctx->result_remove.clear();
   ctx->result_unauth.clear();
   ctx->result_downloads.clear();
   ctx->last_error.clear();
   ctx->cancelled = false;
}

pkgCache::PkgIterator find_pkg(pkgCache &cache, std::string const &key) {
   return cache.FindPkg(key);
}

pkgCache::VerIterator find_version(pkgCache &cache, pkgCache::PkgIterator pkg,
                                   std::string const &version,
                                   std::string const &arch,
                                   std::string const &path,
                                   bool require_local_path) {
   pkgCache::VerIterator fallback(cache);
   for (pkgCache::VerIterator ver = pkg.VersionList(); !ver.end(); ++ver) {
      if (version != ver.VerStr())
         continue;
      if (!arch.empty() && arch != ver.Arch())
         continue;
      bool local_match = path.empty();
      bool local_any = false;
      for (pkgCache::VerFileIterator file = ver.FileList(); !file.end(); ++file) {
         bool local = file.File().Flagged(pkgCache::Flag::LocalSource);
         if (local)
            local_any = true;
         if (local && !path.empty() && path == file.File().FileName()) {
            local_match = true;
            break;
         }
      }
      if (require_local_path) {
         if (local_match)
            return ver;
      } else if (fallback.end()) {
         if (path.empty() || local_any)
            fallback = ver;
      }
   }
   return fallback;
}

bool version_has_repository_file(pkgCache::VerIterator const &ver) {
   for (pkgCache::VerFileIterator file = ver.FileList(); !file.end(); ++file) {
      if (file.File().Flagged(pkgCache::Flag::LocalSource) ||
          file.File().Flagged(pkgCache::Flag::NotSource))
         continue;
      return true;
   }
   return false;
}

bool version_has_local_file(pkgCache::VerIterator const &ver) {
   for (pkgCache::VerFileIterator file = ver.FileList(); !file.end(); ++file)
      if (file.File().Flagged(pkgCache::Flag::LocalSource))
         return true;
   return false;
}

bool version_trusted(pkgCacheFile &cachefile, pkgCache::VerIterator const &ver) {
   pkgSourceList *sources = cachefile.GetSourceList();
   pkgCache *cache = cachefile.GetPkgCache();
   if (sources == nullptr || cache == nullptr)
      return false;

   bool checked = false;
   for (pkgCache::VerFileIterator file = ver.FileList(); !file.end(); ++file) {
      if (file.File().Flagged(pkgCache::Flag::LocalSource) ||
          file.File().Flagged(pkgCache::Flag::NotSource))
         continue;
      checked = true;
      pkgIndexFile *index = nullptr;
      if (!sources->FindIndex(file.File(), index) || index == nullptr ||
          !index->IsTrusted())
         return false;
   }
   /* A purely local version is trusted: fdebi is asked to install that file. */
   return checked ? true : version_has_local_file(ver);
}

std::string version_key(pkgCache::PkgIterator const &pkg) {
   return pkg.FullName(true);
}

void append_field(std::string &out, std::string const &value) {
   out += value;
}

void append_record(std::string &out, std::initializer_list<std::string> fields) {
   bool first = true;
   for (std::string const &field : fields) {
      if (!first)
         out.push_back('\t');
      out += field;
      first = false;
   }
   out.push_back('\n');
}

std::string kind_for(pkgCache &cache, std::string const &old_version,
                     std::string const &new_version) {
   if (old_version.empty())
      return "new";
   int cmp = cache.VS->CompareVersions(old_version.c_str(), new_version.c_str());
   if (cmp == 0)
      return "reinstall";
   return cmp < 0 ? "upgrade" : "downgrade";
}

bool split_tabs(std::string const &line, std::vector<std::string> &fields,
                size_t expected) {
   fields.clear();
   size_t start = 0;
   while (true) {
      size_t pos = line.find('\t', start);
      if (pos == std::string::npos) {
         fields.push_back(line.substr(start));
         break;
      }
      fields.push_back(line.substr(start, pos - start));
      start = pos + 1;
   }
   return fields.size() == expected;
}

class FetchStatus : public pkgAcquireStatus {
   fdebi_fetch_cb callback;
   void *user;
   bool wants_cancel = false;

public:
   FetchStatus(fdebi_fetch_cb callback, void *user)
       : callback(callback), user(user) {}

   bool Pulse(pkgAcquire *owner) override {
      if (!pkgAcquireStatus::Pulse(owner)) {
         wants_cancel = true;
         return false;
      }
      if (callback != nullptr &&
          callback(user, CurrentBytes, TotalBytes) != 0) {
         wants_cancel = true;
         return false;
      }
      return true;
   }

   bool MediaChange(std::string, std::string) override { return false; }

   bool cancelled() const { return wants_cancel; }
};

/* Collect the install set of a solved depcache.  Priors are batch members
 * which were already accepted; they are part of the model but belong to an
 * earlier result. */
struct InstallEntry {
   pkgCache::PkgIterator pkg;
   pkgCache::VerIterator ver;
   std::string key;
   std::string old_version;
   std::string new_version;
   std::string kind;
};

bool solve_depcache(FdebiApt *ctx, pkgDepCache &depcache,
                    std::vector<pkgCache::PkgIterator> const &targets,
                    std::vector<pkgCache::PkgIterator> const &priors,
                    std::set<std::string> const &prior_keys,
                    std::vector<InstallEntry> &installs,
                    std::vector<std::string> &removes) {
   pkgProblemResolver resolver(&depcache);
   for (pkgCache::PkgIterator const &pkg : targets) {
      resolver.Clear(pkg);
      resolver.Protect(pkg);
      depcache.MarkInstall(pkg, false, 0, true);
   }
   for (pkgCache::PkgIterator const &pkg : targets)
      depcache.MarkInstall(pkg, true);

   bool ok = resolver.Resolve(true);
   if (!ok || depcache.BrokenCount() != 0) {
      ctx->last_error = take_errors();
      _error->Discard();
      return false;
   }

   pkgCache *cache = ctx->cache->GetPkgCache();
   std::set<std::string> seen;
   for (pkgCache::PkgIterator pkg = cache->PkgBegin(); !pkg.end(); ++pkg) {
      pkgDepCache::StateCache &state = depcache[pkg];
      if (state.Install() && !state.InstallVer.end()) {
         std::string key = version_key(pkg);
         if (prior_keys.count(key) == 0 && seen.insert(key).second) {
            std::string old_version =
                pkg.CurrentVer().end() ? "" : pkg.CurrentVer().VerStr();
            std::string new_version = state.InstallVer.VerStr();
            installs.push_back(InstallEntry{pkg, state.InstallVer, key,
                                            old_version, new_version,
                                            kind_for(*cache, old_version,
                                                     new_version)});
         }
      }
      if (state.Delete() && !pkg.CurrentVer().end()) {
         std::string key = version_key(pkg);
         if (prior_keys.count(key) == 0)
            removes.push_back(key);
      }
   }

   /* Reinstalling the same version is marked Keep by apt, but fdebi still runs
    * dpkg -i on the local file, so report the requested package explicitly. */
   pkgCache::PkgIterator requested = targets.back();
   std::string requested_key = version_key(requested);
   if (prior_keys.count(requested_key) == 0 &&
       seen.insert(requested_key).second) {
      pkgDepCache::StateCache &state = depcache[requested];
      pkgCache::VerIterator ver =
          state.InstallVer.end() ? state.CandidateVer : state.InstallVer;
      if (!ver.end()) {
         std::string old_version =
             requested.CurrentVer().end() ? "" : requested.CurrentVer().VerStr();
         installs.push_back(InstallEntry{requested, ver, requested_key,
                                         old_version, ver.VerStr(),
                                         kind_for(*cache, old_version,
                                                  ver.VerStr())});
      }
   }
   return true;
}

} // namespace

extern "C" {

FdebiApt *fdebi_apt_open(void) {
   FdebiApt *ctx = new (std::nothrow) FdebiApt();
   if (ctx == nullptr)
      return nullptr;

   _error->Discard();
   _config->Clear();

   if (!pkgInitConfig(*_config)) {
      _error->Discard();
      delete ctx;
      return nullptr;
   }

   /* Match apt's own configuration loading.  Missing files are normal. */
   std::string main_conf = _config->FindFile("Dir::Etc::main");
   std::string conf_parts = _config->FindDir("Dir::Etc::parts");
   if (!main_conf.empty())
      ReadConfigFile(*_config, main_conf);
   if (!conf_parts.empty())
      ReadConfigDir(*_config, conf_parts);
   _error->Discard();

   if (!pkgInitSystem(*_config, _system)) {
      _error->Discard();
      delete ctx;
      return nullptr;
   }
   _error->Discard();

   ctx->cache.reset(new pkgCacheFile());
   return ctx;
}

void fdebi_apt_close(FdebiApt *ctx) { delete ctx; }

int fdebi_apt_set_option(FdebiApt *ctx, const char *key, const char *value) {
   if (ctx == nullptr || key == nullptr || value == nullptr)
      return -1;
   if (ctx->built)
      return -1;
   _config->Set(key, value);
   ctx->options.emplace_back(key, value);
   return 0;
}

int fdebi_apt_add_volatile(FdebiApt *ctx, const char *path) {
   if (ctx == nullptr || path == nullptr || ctx->built)
      return -1;
   ctx->volatile_files.emplace_back(path);
   return 0;
}

int fdebi_apt_build(FdebiApt *ctx) {
   if (ctx == nullptr)
      return -1;
   if (ctx->built)
      return 0;

   _error->Discard();
   if (!ctx->cache->BuildSourceList(nullptr)) {
      fail_apt(ctx, "cannot read apt source list");
      return -1;
   }

   pkgSourceList *sources = ctx->cache->GetSourceList();
   if (sources == nullptr) {
      fail(ctx, "apt source list unavailable");
      return -1;
   }
   for (std::string const &path : ctx->volatile_files) {
      if (!sources->AddVolatileFile(path)) {
         fail_apt(ctx, "cannot add local package to apt cache");
         return -1;
      }
   }

   /* Read-only analysis must not take the dpkg lock: the daemon runs as root
    * but the CLI/GUI may only be inspecting a file. */
   if (!ctx->cache->BuildCaches(nullptr, false)) {
      fail_apt(ctx, "cannot build apt package cache");
      return -1;
   }
   if (!ctx->cache->BuildPolicy(nullptr)) {
      fail_apt(ctx, "cannot build apt policy");
      return -1;
   }
   _error->Discard();
   ctx->built = true;
   return 0;
}

const char *fdebi_apt_native_arch(FdebiApt *ctx) {
   if (ctx == nullptr)
      return "";
   ctx->scratch = _config->Find("APT::Architecture");
   return ctx->scratch.c_str();
}

const char *fdebi_apt_foreign_arches(FdebiApt *ctx) {
   if (ctx == nullptr)
      return "";
   std::string native = _config->Find("APT::Architecture");
   ctx->scratch.clear();
   for (std::string const &arch : APT::Configuration::getArchitectures()) {
      if (arch == native || arch.empty())
         continue;
      if (!ctx->scratch.empty())
         ctx->scratch.push_back('\n');
      ctx->scratch += arch;
   }
   return ctx->scratch.c_str();
}

const char *fdebi_apt_installed_version(FdebiApt *ctx, const char *key) {
   if (ctx == nullptr || key == nullptr || !ctx->built)
      return "";
   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgCache::PkgIterator pkg = find_pkg(*cache, key);
   ctx->scratch.clear();
   if (!pkg.end() && !pkg.CurrentVer().end())
      ctx->scratch = pkg.CurrentVer().VerStr();
   return ctx->scratch.c_str();
}

const char *fdebi_apt_repo_version(FdebiApt *ctx, const char *key) {
   if (ctx == nullptr || key == nullptr || !ctx->built)
      return "";
   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgCache::PkgIterator pkg = find_pkg(*cache, key);
   ctx->scratch.clear();
   pkgCache::VerIterator best(*cache);
   if (!pkg.end()) {
      for (pkgCache::VerIterator ver = pkg.VersionList(); !ver.end(); ++ver) {
         if (!version_has_repository_file(ver))
            continue;
         if (best.end() ||
             cache->VS->CompareVersions(ver.VerStr(), best.VerStr()) > 0)
            best = ver;
      }
   }
   if (!best.end())
      ctx->scratch = best.VerStr();
   return ctx->scratch.c_str();
}

const char *fdebi_apt_conffiles(FdebiApt *ctx, const char *key) {
   if (ctx == nullptr || key == nullptr || !ctx->built)
      return "";
   ctx->scratch.clear();
   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgCache::PkgIterator pkg = find_pkg(*cache, key);
   if (pkg.end() || pkg.CurrentVer().end())
      return ctx->scratch.c_str();

   pkgRecords records(*cache);
   std::string field;
   for (pkgCache::VerFileIterator file = pkg.CurrentVer().FileList();
        !file.end(); ++file) {
      pkgRecords::Parser &parser = records.Lookup(file);
      field = parser.RecordField("Conffiles");
      if (!field.empty())
         break;
   }
   if (field.empty())
      return ctx->scratch.c_str();

   std::istringstream stream(field);
   std::string line;
   while (std::getline(stream, line)) {
      std::istringstream tokens(line);
      std::string path;
      std::string md5;
      if (!(tokens >> path >> md5))
         continue;
      if (path.empty() || md5.empty())
         continue;
      ctx->scratch += path;
      ctx->scratch.push_back('\t');
      ctx->scratch += md5;
      ctx->scratch.push_back('\n');
   }
   return ctx->scratch.c_str();
}

const char *fdebi_apt_broken(FdebiApt *ctx) {
   if (ctx == nullptr || !ctx->built)
      return "";
   ctx->scratch.clear();
   pkgCache *cache = ctx->cache->GetPkgCache();
   std::set<std::string> broken;

   for (pkgCache::PkgIterator pkg = cache->PkgBegin(); !pkg.end(); ++pkg) {
      if (pkg->CurrentVer == 0)
         continue;
      switch (pkg->CurrentState) {
      case pkgCache::State::UnPacked:
      case pkgCache::State::HalfConfigured:
      case pkgCache::State::HalfInstalled:
      case pkgCache::State::TriggersAwaited:
      case pkgCache::State::TriggersPending:
         broken.insert(version_key(pkg));
         break;
      default:
         break;
      }
   }

   pkgDepCache depcache(cache, ctx->cache->GetPolicy());
   if (depcache.Init(nullptr)) {
      for (pkgCache::PkgIterator pkg = cache->PkgBegin(); !pkg.end(); ++pkg) {
         if (pkg->CurrentVer != 0 && depcache[pkg].InstBroken())
            broken.insert(version_key(pkg));
      }
   }
   _error->Discard();

   for (std::string const &key : broken) {
      ctx->scratch += key;
      ctx->scratch.push_back('\n');
   }
   return ctx->scratch.c_str();
}

int fdebi_apt_solve(FdebiApt *ctx, const char *name, const char *version,
                    const char *arch, const char *path,
                    const char *const *priors, size_t n_priors) {
   if (ctx == nullptr || name == nullptr || version == nullptr ||
       arch == nullptr || !ctx->built)
      return -1;
   clear_results(ctx);

   std::string package_name(name);
   std::string package_version(version);
   std::string package_arch(arch);
   std::string package_path(path == nullptr ? "" : path);
   if (package_name.empty() || package_version.empty())
      return 2;

   std::vector<std::string> architectures =
       APT::Configuration::getArchitectures();
   if (package_arch != "all" &&
       std::find(architectures.begin(), architectures.end(), package_arch) ==
           architectures.end())
      return 3;

   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgDepCache depcache(cache, ctx->cache->GetPolicy());
   if (!depcache.Init(nullptr)) {
      fail_apt(ctx, "cannot initialise apt dependency cache");
      return -1;
   }

   std::string key;
   if (package_arch == "all" ||
       package_arch == _config->Find("APT::Architecture"))
      key = package_name;
   else
      key = package_name + ":" + package_arch;

   pkgCache::PkgIterator requested = find_pkg(*cache, key);
   if (requested.end()) {
      fail(ctx, "local package missing from apt cache");
      return -1;
   }
   if (!requested.CurrentVer().end() &&
       cache->VS->CompareVersions(requested.CurrentVer().VerStr(),
                                  package_version.c_str()) > 0)
      return 4;

   pkgCache::VerIterator requested_ver =
       find_version(*cache, requested, package_version, package_arch,
                    package_path, true);
   if (requested_ver.end())
      requested_ver = find_version(*cache, requested, package_version,
                                   package_arch, package_path, false);
   if (requested_ver.end()) {
      fail(ctx, "local package version missing from apt cache");
      return -1;
   }

   std::set<std::string> prior_keys;
   std::vector<pkgCache::PkgIterator> targets;
   for (size_t i = 0; i < n_priors; ++i) {
      if (priors == nullptr || priors[i] == nullptr)
         return -1;
      std::vector<std::string> fields;
      if (!split_tabs(priors[i], fields, 4))
         return -1;
      std::string const &prior_key = fields[0];
      std::string const &prior_version = fields[1];
      std::string const &prior_arch = fields[2];
      std::string const &prior_path = fields[3];
      pkgCache::PkgIterator prior_pkg = find_pkg(*cache, prior_key);
      if (prior_pkg.end()) {
         fail(ctx, "batch package missing from apt cache: " + prior_key);
         return -1;
      }
      pkgCache::VerIterator prior_ver =
          find_version(*cache, prior_pkg, prior_version, prior_arch,
                       prior_path, true);
      if (prior_ver.end())
         prior_ver = find_version(*cache, prior_pkg, prior_version, prior_arch,
                                  prior_path, false);
      if (prior_ver.end()) {
         fail(ctx, "batch package version missing from apt cache: " +
                       prior_key);
         return -1;
      }
      depcache.SetCandidateVersion(prior_ver);
      targets.push_back(prior_pkg);
      prior_keys.insert(version_key(prior_pkg));
   }

   depcache.SetCandidateVersion(requested_ver);
   targets.push_back(requested);

   std::vector<InstallEntry> installs;
   std::vector<std::string> removes;
   if (!solve_depcache(ctx, depcache, targets, targets, prior_keys, installs,
                       removes))
      return 1;

   for (InstallEntry const &entry : installs) {
      append_record(ctx->result_install,
                    {entry.key, entry.kind, entry.old_version,
                     entry.new_version});
   }
   for (std::string const &removed : removes)
      append_record(ctx->result_remove, {removed});

   std::set<std::string> seen_downloads;
   std::set<std::string> seen_unauth;
   for (InstallEntry const &entry : installs) {
      pkgCache::VerIterator ver = entry.ver;
      if (ver.end())
         continue;
      if (entry.key != key && version_has_repository_file(ver)) {
         std::string download_key = entry.key + "\t" + ver.VerStr();
         if (seen_downloads.insert(download_key).second) {
            unsigned long long size =
                static_cast<unsigned long long>(ver->Size);
            append_record(ctx->result_downloads,
                          {entry.key, ver.VerStr(), std::to_string(size),
                           version_trusted(*ctx->cache, ver) ? "1" : "0"});
         }
      }
      if (entry.key != key && !version_trusted(*ctx->cache, ver) &&
          seen_unauth.insert(entry.key).second)
         append_record(ctx->result_unauth, {entry.key});
   }

   _error->Discard();
   return 0;
}

const char *fdebi_apt_result_install(FdebiApt *ctx) {
   return ctx == nullptr ? "" : ctx->result_install.c_str();
}
const char *fdebi_apt_result_remove(FdebiApt *ctx) {
   return ctx == nullptr ? "" : ctx->result_remove.c_str();
}
const char *fdebi_apt_result_unauth(FdebiApt *ctx) {
   return ctx == nullptr ? "" : ctx->result_unauth.c_str();
}
const char *fdebi_apt_result_downloads(FdebiApt *ctx) {
   return ctx == nullptr ? "" : ctx->result_downloads.c_str();
}

const char *fdebi_apt_last_error(FdebiApt *ctx) {
   return ctx == nullptr ? "" : ctx->last_error.c_str();
}

char *fdebi_apt_fetch(FdebiApt *ctx, const char *name, const char *version,
                      fdebi_fetch_cb cb, void *user) {
   if (ctx == nullptr || name == nullptr || version == nullptr || !ctx->built)
      return nullptr;
   ctx->cancelled = false;
   ctx->last_error.clear();
   _error->Discard();

   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgCache::PkgIterator pkg = find_pkg(*cache, name);
   if (pkg.end()) {
      fail(ctx, "package missing from apt cache");
      return nullptr;
   }
   pkgCache::VerIterator ver =
       find_version(*cache, pkg, version, std::string(), std::string(), false);
   if (ver.end()) {
      fail(ctx, "package version missing from apt cache");
      return nullptr;
   }
   if (!version_has_repository_file(ver)) {
      fail(ctx, "package has no repository archive");
      return nullptr;
   }

   pkgRecords records(*cache);
   FetchStatus status(cb, user);
   pkgAcquire fetcher(&status);
   std::string store;
   new pkgAcqArchive(&fetcher, ctx->cache->GetSourceList(), &records, ver,
                     store);
   pkgAcquire::RunResult result = fetcher.Run(200000);
   if (status.cancelled() || result == pkgAcquire::Cancelled) {
      ctx->cancelled = true;
      fetcher.Shutdown();
      _error->Discard();
      return nullptr;
   }
   if (result != pkgAcquire::Continue || store.empty() ||
       access(store.c_str(), R_OK) != 0) {
      fail_apt(ctx, "cannot fetch package archive");
      fetcher.Shutdown();
      return nullptr;
   }
   char *copy = ::strdup(store.c_str());
   fetcher.Shutdown();
   _error->Discard();
   return copy;
}

int fdebi_apt_cancelled(FdebiApt *ctx) {
   return ctx == nullptr ? 0 : (ctx->cancelled ? 1 : 0);
}

const char *fdebi_apt_reverse_deps(FdebiApt *ctx, const char *removed) {
   if (ctx == nullptr || removed == nullptr || !ctx->built)
      return "";
   ctx->scratch.clear();
   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgCache::PkgIterator removed_pkg = find_pkg(*cache, removed);
   if (removed_pkg.end() || removed_pkg.CurrentVer().end())
      return ctx->scratch.c_str();
   std::string removed_key = version_key(removed_pkg);

   for (pkgCache::PkgIterator pkg = cache->PkgBegin(); !pkg.end(); ++pkg) {
      if (pkg.CurrentVer().end() || version_key(pkg) == removed_key)
         continue;
      std::string requires;
      for (pkgCache::DepIterator dep = pkg.CurrentVer().DependsList();
           !dep.end();) {
         pkgCache::DepIterator start;
         pkgCache::DepIterator end;
         dep.GlobOr(start, end);
         if (!start.IsCritical() || start.IsNegative())
            continue;

         bool needed_by_removed = false;
         bool satisfied_by_other = false;
         for (pkgCache::DepIterator alt = start;; ++alt) {
            pkgCache::PkgIterator target = alt.TargetPkg();
            if (!target.end() && !target.CurrentVer().end() &&
                alt.IsSatisfied(target.CurrentVer())) {
               if (version_key(target) == removed_key)
                  needed_by_removed = true;
               else
                  satisfied_by_other = true;
            }
            pkgCache::PkgIterator provide_target = alt.TargetPkg();
            for (pkgCache::PrvIterator prv = provide_target.ProvidesList();
                 !prv.end(); ++prv) {
               pkgCache::PkgIterator owner = prv.OwnerPkg();
               if (owner.CurrentVer().end() ||
                   !(prv.OwnerVer() == owner.CurrentVer()) ||
                   !alt.IsSatisfied(prv))
                  continue;
               if (version_key(owner) == removed_key)
                  needed_by_removed = true;
               else
                  satisfied_by_other = true;
            }
            if (alt == end)
               break;
         }

         if (needed_by_removed && !satisfied_by_other) {
            if (!requires.empty())
               requires += " | ";
            requires += start.TargetPkg().FullName(true);
         }
      }
      if (!requires.empty())
         append_record(ctx->scratch, {version_key(pkg), requires});
   }
   return ctx->scratch.c_str();
}

int fdebi_apt_mark(FdebiApt *ctx, const char *const *auto_names,
                   size_t n_auto, const char *manual) {
   if (ctx == nullptr || !ctx->built)
      return -1;
   ctx->last_error.clear();
   _error->Discard();
   pkgCache *cache = ctx->cache->GetPkgCache();
   pkgDepCache depcache(cache, ctx->cache->GetPolicy());
   if (!depcache.Init(nullptr)) {
      fail_apt(ctx, "cannot initialise apt dependency cache");
      return -1;
   }

   for (size_t i = 0; i < n_auto; ++i) {
      if (auto_names == nullptr || auto_names[i] == nullptr)
         continue;
      pkgCache::PkgIterator pkg = find_pkg(*cache, auto_names[i]);
      if (!pkg.end() && !pkg.CurrentVer().end())
         depcache.MarkAuto(pkg, true);
   }
   if (manual != nullptr && manual[0] != '\0') {
      pkgCache::PkgIterator pkg = find_pkg(*cache, manual);
      if (!pkg.end() && !pkg.CurrentVer().end())
         depcache.MarkAuto(pkg, false);
   }

   if (!depcache.writeStateFile(nullptr)) {
      fail_apt(ctx, "cannot write apt extended states");
      return -1;
   }
   _error->Discard();
   return 0;
}

void fdebi_apt_free_string(char *s) { ::free(s); }

} // extern "C"
