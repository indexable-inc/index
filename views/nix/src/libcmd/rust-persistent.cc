#include "nix/cmd/rust-persistent.hh"
#include "nix/cmd/rust-command.hh"
#include "nix/fetchers/git-utils.hh"
#include "nix/util/posix-source-accessor.hh"
#include "nix/util/finally.hh"
#include "nix/util/users.hh"
#include "nix/util/file-descriptor.hh"
#include "nix/util/signals.hh"
#include "ixe-persistent.h"

#include <ctime>
#include <cerrno>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace nix {

namespace {

uint64_t clockNs(clockid_t clock)
{
    struct timespec value;
    if (clock_gettime(clock, &value) != 0)
        throw SysError("reading persistent evaluator clock");
    return uint64_t(value.tv_sec) * 1000000000ull + uint64_t(value.tv_nsec);
}

IxeBytes bytes(const std::string & text)
{
    return {reinterpret_cast<const unsigned char *>(text.data()), text.size()};
}

void checkRequestStatus(int status, char * error)
{
    std::unique_ptr<char, decltype(&ixe_string_free)> owned(error, ixe_string_free);
    if (status != 0)
        throw Error("persistent request file: %s", error ? error : "invalid Rust protocol call");
}

std::string readRequestFile(const std::filesystem::path & path)
{
    AutoCloseFD fd(open(path.c_str(), O_RDONLY | O_CLOEXEC | O_NONBLOCK));
    if (!fd)
        throw SysError("opening persistent request file '%s'", path.string());
    struct stat info;
    if (fstat(fd.get(), &info) != 0)
        throw SysError("inspecting persistent request file '%s'", path.string());
    if (!S_ISREG(info.st_mode))
        throw Error("persistent request file must be a regular file");
    const auto limit = ixe_persistent_limits().file_bytes;
    std::string result;
    char buffer[4096];
    while (true) {
        checkInterrupt();
        auto count = ::read(fd.get(), buffer, sizeof(buffer));
        if (count < 0) {
            if (errno == EINTR)
                continue;
            throw SysError("reading persistent request file '%s'", path.string());
        }
        if (count == 0)
            return result;
        if (size_t(count) > limit - result.size())
            throw Error("persistent request file exceeds %d bytes", limit);
        result.append(buffer, size_t(count));
    }
}

} // namespace

std::string
rustEvalPersistentRequest(
    SourceExprCommand & command,
    const std::string & installable,
    RustEvalCache & cache,
    const std::optional<std::string> & apply)
{
    RustEvalCommandOptions options{
        .json = true,
        .file = command.file.has_value(),
        .expr = command.expr.has_value(),
        .installable = installable,
    };
    rustValidateEvalCommand(options);
    auto cacheBefore = cache.stats();
    auto wallStart = clockNs(CLOCK_MONOTONIC);
    auto cpuStart = clockNs(CLOCK_PROCESS_CPUTIME_ID);

    // These host caches describe mutable filesystem state. Refresh them before
    // reading --file or resolving a new flake, even when the text is unchanged.
    GitRepo::clearCachedWorkdirInfo();
    PosixSourceAccessor::clearCache();
    if (evalSettings.evalCacheDir.get().empty())
        evalSettings.evalCacheDir = (getCacheDir() / "eval").string();
    auto source = rustSourceOf(command);
    auto state = command.getEvalState();
    auto evicted = state->prepareForNextRequest();
    auto evaluand = rustEvaluandOf(command, state, source, installable);
    evaluand.apply = apply;
    auto value = rustEvalCommand(*state, evaluand, options, &cache);
    auto cacheAfter = cache.stats();

    char * output = nullptr;
    char * error = nullptr;
    Finally release([&]() {
        ixe_string_free(output);
        ixe_string_free(error);
    });
    IxePersistentReportInput input{
        .installable = bytes(installable),
        .value_json = bytes(value),
        .wall_ns = clockNs(CLOCK_MONOTONIC) - wallStart,
        .cpu_ns = clockNs(CLOCK_PROCESS_CPUTIME_ID) - cpuStart,
        .inputs_evicted = evicted,
        .witness_memory_hits = cacheAfter.memoryHits - cacheBefore.memoryHits,
        .witness_disk_loads = cacheAfter.diskLoads - cacheBefore.diskLoads,
        .witness_cache_bytes = cacheAfter.retainedBytes,
        .witness_cache_entries = cacheAfter.entries,
    };
    if (ixe_persistent_report(input, &output, &error) != 0)
        throw Error("cannot report persistent evaluation: %s", error ? error : "invalid Rust report call");
    if (!output)
        throw Error("Rust persistent evaluation report returned no output");
    return output;
}

RustPersistentRequests::RustPersistentRequests(const std::filesystem::path & path)
{
    auto input = readRequestFile(path);
    char * error = nullptr;
    auto status = ixe_persistent_requests_new(bytes(input), &requests, &error);
    checkRequestStatus(status, error);
}

RustPersistentRequests::~RustPersistentRequests()
{
    ixe_persistent_requests_free(requests);
}

std::optional<RustPersistentRequest> RustPersistentRequests::next()
{
    IxePersistentRequestView request{};
    bool done = false;
    char * error = nullptr;
    auto status = ixe_persistent_requests_next(requests, &request, &done, &error);
    checkRequestStatus(status, error);
    if (done)
        return std::nullopt;
    auto copy = [](IxeBytes text) {
        return std::string(reinterpret_cast<const char *>(text.text), text.len);
    };
    return RustPersistentRequest{
        .installable = copy(request.installable),
        .apply = request.has_apply ? std::optional(copy(request.apply)) : std::nullopt,
    };
}

std::string RustPersistentRequests::complete(bool success, const std::string & payload)
{
    char * output = nullptr;
    char * error = nullptr;
    auto status = ixe_persistent_requests_complete(requests, success, bytes(payload), &output, &error);
    std::unique_ptr<char, decltype(&ixe_string_free)> owned(output, ixe_string_free);
    checkRequestStatus(status, error);
    if (!output)
        throw Error("persistent request framing returned no output");
    return output;
}

} // namespace nix
