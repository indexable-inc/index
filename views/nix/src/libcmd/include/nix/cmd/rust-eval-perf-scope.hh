#pragma once

#include <cstddef>

namespace nix {

/// Host reentry shares the outer question's Rust counters. A member guard also
/// balances nesting when construction of the rest of the host setup throws.
struct RustEvalPerfScope
{
    const bool outermost;

    explicit RustEvalPerfScope(void (*reset)())
        : outermost(depth == 0)
    {
        if (outermost)
            reset();
        ++depth;
    }

    ~RustEvalPerfScope()
    {
        --depth;
    }

    RustEvalPerfScope(const RustEvalPerfScope &) = delete;
    RustEvalPerfScope & operator=(const RustEvalPerfScope &) = delete;

private:
    static inline thread_local std::size_t depth = 0;
};

} // namespace nix
