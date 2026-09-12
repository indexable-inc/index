#include "nix/store/references.hh"
#include <ix-store-stream.h>

#include <algorithm>
#include <exception>
#include <vector>

namespace nix {

namespace {

void checkStream(int32_t status)
{
    switch (status) {
    case 0:
        return;
    case 1:
        throw Error("invalid Rust store stream input");
    case 2:
        throw Error("Rust store stream is failed or finished");
    case 3:
        throw Error("Rust store stream panicked");
    case 4:
        throw Error("Rust store stream output failed");
    default:
        throw Error("unknown Rust store stream status %d", status);
    }
}

IxsBytes byteSpan(std::string_view bytes)
{
    return {reinterpret_cast<const uint8_t *>(bytes.data()), bytes.size()};
}

uint32_t hashAlgorithmId(HashAlgorithm algorithm)
{
    switch (algorithm) {
    case HashAlgorithm::MD5:
        return 0;
    case HashAlgorithm::SHA1:
        return 1;
    case HashAlgorithm::SHA256:
        return 2;
    case HashAlgorithm::SHA512:
        return 3;
    case HashAlgorithm::BLAKE3:
        return 4;
    }
    throw Error("unsupported store stream hash algorithm");
}

} // namespace

struct RefScanSink::State
{
    void * handle = nullptr;
    std::vector<std::string> names;
    std::vector<size_t> indices;
    size_t reported = 0;
    StringSet found;

    ~State()
    {
        ixs_refscan_free(handle);
    }
};

RefScanSink::RefScanSink(StringSet && hashes)
    : state(std::make_unique<State>())
{
    std::string packed;
    for (const auto & hash : hashes) {
        if (hash.size() != 32)
            throw Error("store reference hash must contain 32 bytes");
        state->names.push_back(hash);
        packed += hash;
    }
    auto bytes = byteSpan(packed);
    checkStream(ixs_refscan_new(bytes.data, state->names.size(), &state->handle));
    state->indices.resize(state->names.size());
}

RefScanSink::~RefScanSink() = default;

void RefScanSink::operator()(std::string_view data)
{
    auto bytes = byteSpan(data);
    checkStream(ixs_refscan_feed(state->handle, bytes.data, bytes.len));
}

StringSet & RefScanSink::getResult()
{
    size_t count = 0;
    checkStream(ixs_refscan_result(state->handle, state->indices.data(), state->indices.size(), &count));
    if (count > state->indices.size() || count < state->reported)
        throw Error("Rust store scanner returned too many references");
    for (size_t i = state->reported; i < count; ++i)
        state->found.insert(state->names.at(state->indices.at(i)));
    state->reported = count;
    return state->found;
}

struct RewritingSink::State
{
    void * handle = nullptr;
    Sink & next;
    std::exception_ptr exception;

    explicit State(Sink & next)
        : next(next)
    {
    }

    ~State()
    {
        ixs_rewriter_free(handle);
    }

    static int32_t emit(void * context, const uint8_t * bytes, size_t len) noexcept
    {
        auto & state = *static_cast<State *>(context);
        try {
            state.next(std::string_view(reinterpret_cast<const char *>(bytes), len));
            return 0;
        } catch (...) {
            state.exception = std::current_exception();
            return 1;
        }
    }

    void feed(std::string_view data, bool finish)
    {
        auto bytes = byteSpan(data);
        auto status = ixs_rewriter_feed(handle, bytes.data, bytes.len, finish, emit, this);
        if (exception)
            std::rethrow_exception(exception);
        checkStream(status);
    }
};

RewritingSink::RewritingSink(const std::string & from, const std::string & to, Sink & nextSink)
    : RewritingSink(StringMap{{from, to}}, nextSink)
{
}

RewritingSink::RewritingSink(const StringMap & rewrites, Sink & nextSink)
    : state(std::make_unique<State>(nextSink))
{
    std::vector<IxsRewrite> rules;
    rules.reserve(rewrites.size());
    for (const auto & [from, to] : rewrites)
        rules.push_back({byteSpan(from), byteSpan(to)});
    checkStream(ixs_rewriter_new(rules.data(), rules.size(), &state->handle));
}

RewritingSink::~RewritingSink() = default;

void RewritingSink::operator()(std::string_view data)
{
    state->feed(data, false);
}

void RewritingSink::flush()
{
    state->feed({}, true);
}

struct HashModuloSink::State
{
    void * handle = nullptr;
    Hash hash;

    explicit State(HashAlgorithm algorithm)
        : hash(algorithm)
    {
    }

    ~State()
    {
        ixs_modulo_free(handle);
    }
};

HashModuloSink::HashModuloSink(HashAlgorithm algorithm, const std::string & modulus)
    : state(std::make_unique<State>(algorithm))
{
    auto bytes = byteSpan(modulus);
    checkStream(ixs_modulo_new(hashAlgorithmId(algorithm), bytes.data, bytes.len, &state->handle));
}

HashModuloSink::~HashModuloSink() = default;

void HashModuloSink::operator()(std::string_view data)
{
    auto bytes = byteSpan(data);
    checkStream(ixs_modulo_feed(state->handle, bytes.data, bytes.len));
}

HashResult HashModuloSink::finish()
{
    IxsDigest digest{};
    checkStream(ixs_modulo_finish(state->handle, &digest));
    if (digest.len != state->hash.hashSize)
        throw Error("Rust store stream returned an invalid digest length");
    std::copy_n(digest.bytes, digest.len, state->hash.hash);
    return {.hash = state->hash, .numBytesDigested = digest.input_bytes};
}

} // namespace nix
