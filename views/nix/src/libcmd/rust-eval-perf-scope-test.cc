#include "nix/cmd/rust-eval-perf-scope.hh"

#include <cstdio>
#include <stdexcept>

namespace {

unsigned resets = 0;
unsigned work = 0;

void reset()
{
    ++resets;
    work = 0;
}

void check(bool condition, const char * message)
{
    if (!condition)
        throw std::runtime_error(message);
}

struct ThrowingSetup
{
    nix::RustEvalPerfScope scope{reset};

    ThrowingSetup()
    {
        throw std::runtime_error("setup failed after constructing scope");
    }
};

void failSetup()
{
    bool threw = false;
    try {
        ThrowingSetup setup;
    } catch (const std::runtime_error &) {
        threw = true;
    }
    check(threw, "test setup did not throw");
}

} // namespace

int main()
{
    try {
        {
            nix::RustEvalPerfScope outer(reset);
            check(outer.outermost && resets == 1, "outer scope did not reset once");
            work += 11;
            {
                nix::RustEvalPerfScope inner(reset);
                check(!inner.outermost && resets == 1, "nested scope reset outer work");
                work += 7;
            }
            check(work == 18, "nested scope lost work");
            failSetup();
            nix::RustEvalPerfScope afterFailure(reset);
            check(!afterFailure.outermost && resets == 1 && work == 18, "nested construction failure broke scope");
        }
        failSetup();
        check(resets == 2, "failed outer setup did not start a new measurement");
        {
            nix::RustEvalPerfScope next(reset);
            check(next.outermost && resets == 3 && work == 0, "construction failure leaked nesting into next request");
        }
        std::puts("Rust performance scope: nested work retained and failed setup balanced");
        return 0;
    } catch (const std::exception & error) {
        std::fprintf(stderr, "%s\n", error.what());
        return 1;
    }
}
