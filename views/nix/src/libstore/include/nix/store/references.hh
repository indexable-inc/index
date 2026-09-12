#pragma once
///@file

#include "nix/util/hash.hh"

namespace nix {

class RefScanSink : public Sink
{
    struct State;
    std::unique_ptr<State> state;

public:

    RefScanSink(StringSet && hashes);
    ~RefScanSink() override;

    StringSet & getResult();

    void operator()(std::string_view data) override;
};

struct RewritingSink : Sink
{
private:
    struct State;
    std::unique_ptr<State> state;

public:

    RewritingSink(const std::string & from, const std::string & to, Sink & nextSink);
    RewritingSink(const StringMap & rewrites, Sink & nextSink);
    ~RewritingSink() override;

    void operator()(std::string_view data) override;

    void flush();
};

struct HashModuloSink : AbstractHashSink
{
private:
    struct State;
    std::unique_ptr<State> state;

public:

    HashModuloSink(HashAlgorithm ha, const std::string & modulus);
    ~HashModuloSink() override;

    void operator()(std::string_view data) override;

    HashResult finish() override;
};

} // namespace nix
