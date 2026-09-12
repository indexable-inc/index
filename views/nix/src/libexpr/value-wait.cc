#include "nix/expr/eval.hh"
#include "nix/expr/value-wait.hh"
#include "nix/util/sync.hh"

#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>

namespace nix {

struct alignas(64) WaiterDomain
{
    std::condition_variable cv;
};

static std::array<Sync<WaiterDomain>, 128> waiterDomains;

std::unique_ptr<InterruptCallback> registerValueInterruptCallback()
{
    return createInterruptCallback([]() {
        for (auto & domain : waiterDomains)
            domain.lock()->cv.notify_all();
    });
}

static Sync<WaiterDomain> & getWaiterDomain(detail::ValueBase & v)
{
    auto domain = (((size_t) &v) >> 5) % waiterDomains.size();
    return waiterDomains[domain];
}

static std::atomic<uint32_t> nextEvalThreadId{1};
[[gnu::tls_model("initial-exec")]] thread_local uint32_t myEvalThreadId(nextEvalThreadId++);

template<>
ValueStorage<sizeof(void *)>::PackedPointer
ValueStorage<sizeof(void *)>::waitOnThunk(EvalState & state, PackedPointer expectedP0)
{
    state.nrThunksAwaited++;

    auto domain = getWaiterDomain(*this).lock();

    auto threadId = expectedP0 >> discriminatorBits;

    if (static_cast<PrimaryDiscriminator>(expectedP0 & discriminatorMask) == pdAwaited) {
        /* Make sure that the value is still awaited, now that we're
           holding the domain lock. */
        auto p0_ = p0.load(std::memory_order_acquire);
        auto pd = static_cast<PrimaryDiscriminator>(p0_ & discriminatorMask);

        /* If the value has been finalized in the meantime (i.e. is no
           longer pending), we're done. */
        if (pd != pdAwaited) {
            assert(pd != pdThunk && pd != pdPending);
            return p0_;
        }
    } else {
        /* Mark this value as being waited on. */
        PackedPointer p0_ = expectedP0;
        if (!p0.compare_exchange_strong(
                p0_,
                pdAwaited | (threadId << discriminatorBits),
                std::memory_order_acquire,
                std::memory_order_acquire)) {
            /* If the value has been finalized in the meantime (i.e. is
               no longer pending), we're done. */
            auto pd = static_cast<PrimaryDiscriminator>(p0_ & discriminatorMask);
            if (pd != pdAwaited) {
                assert(pd != pdThunk && pd != pdPending);
                return p0_;
            }
            /* The value was already in the "waited on" state, so we're
               not the only thread waiting on it. */
        }
    }

    /* Wait for another thread to finish this value. */
    if (threadId == myEvalThreadId)
        state.error<InfiniteRecursionError>("infinite recursion encountered")
            .atPos(((Value &) *this).determinePos(noPos))
            .debugThrow();

    state.nrThunksAwaitedSlow++;
    state.currentlyWaiting++;
    state.maxWaiting = std::max<uint64_t>(state.maxWaiting, state.currentlyWaiting);

    auto now1 = std::chrono::steady_clock::now();

    while (true) {
        domain.wait(domain->cv);
        auto p0_ = p0.load(std::memory_order_acquire);
        auto pd = static_cast<PrimaryDiscriminator>(p0_ & discriminatorMask);
        if (pd != pdAwaited) {
            assert(pd != pdThunk && pd != pdPending);
            auto now2 = std::chrono::steady_clock::now();
            state.microsecondsWaiting += std::chrono::duration_cast<std::chrono::microseconds>(now2 - now1).count();
            state.currentlyWaiting--;
            return p0_;
        }
        state.nrSpuriousWakeups++;
        checkInterrupt();
    }
}

template<>
void ValueStorage<sizeof(void *)>::notifyWaiters()
{
    auto domain = getWaiterDomain(*this).lock();

    domain->cv.notify_all();
}

} // namespace nix
