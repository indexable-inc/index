#include "nix/expr/eval-error.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/value.hh"
#include "nix/store/store-api.hh"

namespace nix {

InvalidPathError::InvalidPathError(EvalState & state, const StorePath & path)
    : CloneableError(state, "path '%s' is not valid", path.to_string())
    , path{path}
{
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::withExitStatus(unsigned int exitStatus)
{
    error.withExitStatus(exitStatus);
    return *this;
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::atPos(PosIdx pos)
{
    error.err.pos = error.state.positions[pos];
    return *this;
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::atPos(std::shared_ptr<const Pos> pos)
{
    error.err.pos = std::move(pos);
    return *this;
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::atPos(Value & value, PosIdx fallback)
{
    return atPos(value.determinePos(fallback));
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::withTrace(PosIdx pos, const std::string_view text)
{
    error.addTrace(error.state.positions[pos], text);
    return *this;
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::withSuggestions(Suggestions & s)
{
    error.err.suggestions = s;
    return *this;
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::addTrace(PosIdx pos, HintFmt hint)
{
    error.addTrace(error.state.positions[pos], hint);
    return *this;
}

template<class T>
template<typename... Args>
EvalErrorBuilder<T> &
EvalErrorBuilder<T>::addTrace(PosIdx pos, std::string_view formatString, const Args &... formatArgs)
{

    addTrace(error.state.positions[pos], HintFmt(std::string(formatString), formatArgs...));
    return *this;
}

template<class T>
EvalErrorBuilder<T> & EvalErrorBuilder<T>::setIsFromExpr()
{
    error.err.isFromExpr = true;
    return *this;
}

template<class T>
void EvalErrorBuilder<T>::debugThrow()
{
    // `EvalState` is the only class that can construct an `EvalErrorBuilder`,
    // and it does so in dynamic storage. This is the final method called on
    // any such instance and must delete itself before throwing the underlying
    // error.
    auto error = std::move(this->error);
    delete this;

    throw error;
}

template<class T>
void EvalErrorBuilder<T>::panic()
{
    logError(error.info());
    printError(
        "This is a bug! An unexpected condition occurred, causing the Nix evaluator to have to stop. If you could share a reproducible example or a core dump, please open an issue at https://github.com/NixOS/nix/issues");
    abort();
}

template class EvalErrorBuilder<EvalBaseError>;
template class EvalErrorBuilder<EvalError>;
template class EvalErrorBuilder<AssertionError>;
template class EvalErrorBuilder<ThrownError>;
template class EvalErrorBuilder<Abort>;
template class EvalErrorBuilder<TypeError>;
template class EvalErrorBuilder<UndefinedVarError>;
template class EvalErrorBuilder<MissingArgumentError>;
template class EvalErrorBuilder<InfiniteRecursionError>;
template class EvalErrorBuilder<StackOverflowError>;
template class EvalErrorBuilder<InvalidPathError>;
template class EvalErrorBuilder<IFDError>;
template class EvalErrorBuilder<RecoverableEvalError>;

} // namespace nix
