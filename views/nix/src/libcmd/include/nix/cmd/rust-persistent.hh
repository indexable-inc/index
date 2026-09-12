#pragma once

#include "nix/cmd/command.hh"
#include "nix/cmd/rust-eval-session.hh"

struct IxePersistentRequests;

namespace nix {

/// Refresh mutable host inputs, execute the production Rust JSON question,
/// and return its JSON-lines report with measured Rust cache/work counters.
std::string
rustEvalPersistentRequest(
    SourceExprCommand & command,
    const std::string & installable,
    RustEvalCache & cache,
    const std::optional<std::string> & apply = std::nullopt);

struct RustPersistentRequest
{
    std::string installable;
    std::optional<std::string> apply;
};

/// Rust owns request validation, ordering and output limits. Host code reads
/// one bounded regular file and copies borrowed request text before evaluation.
class RustPersistentRequests
{
    IxePersistentRequests * requests = nullptr;
public:
    explicit RustPersistentRequests(const std::filesystem::path & path);
    ~RustPersistentRequests();
    RustPersistentRequests(const RustPersistentRequests &) = delete;
    RustPersistentRequests & operator=(const RustPersistentRequests &) = delete;
    std::optional<RustPersistentRequest> next();
    std::string complete(bool success, const std::string & payload);
};

} // namespace nix
