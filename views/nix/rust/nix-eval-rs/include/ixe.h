#pragma once
/* C ABI of nix-eval-rs (rust/nix-eval-rs/src/capi.rs). Hand-written; keep the
 * status values and the type and render enums in step with capi.rs.
 *
 * Two entry points, for two different questions:
 *
 *   ixe_eval_expr        render this whole expression, one call, one string.
 *   ixe_session_*        evaluate, then walk the value: select an attribute
 *                        path, ask what something is, render one part of it.
 *
 * The second exists because the first cannot be lazy. `nix eval -f x.nix
 * a.b.c` must not force a's siblings, and a call whose only output is
 * rendered text has already forced everything by the time it returns.
 *
 * The string API stays beside the handle API for an embedder with one
 * expression and no selection, arguments or applications to name: the
 * crate's own tests and harnesses. Every nix command goes through the
 * session API, whose question is what the memo keys on.
 */

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* An evaluation session: one VM, the values it produced, and the message
 * belonging to its most recent failure. Opaque; only ever held by pointer.
 *
 * Declared here, at the top, rather than beside the handle API further down.
 * It used to sit down there, and `ixe_session_refusal_token` was added above
 * it, so this header stopped compiling for anyone building with
 * `-Dnix:rust-eval=enabled` while the default configuration stayed green. A
 * type every section may want belongs before all of them. */
typedef struct IxeSession IxeSession;
typedef struct IxeEvalCache IxeEvalCache;

typedef struct
{
    uint64_t memory_hits;
    uint64_t disk_loads;
    uint64_t retained_bytes;
    uint64_t entries;
    uint64_t evictions;
} IxeEvalCacheStats;

/* Where a failure happened.
 *
 * line == 0 means there is no position, which is a real answer rather than a
 * missing one: an error raised with none of the user's source on the frame
 * stack has nowhere to point, and cppnix prints no `at ...` line for those
 * either. `file` is a malloc'd path -- free it with ixe_string_free -- or
 * NULL when the source was a string with no file behind it (--expr), which is
 * a different answer from an empty path and not a default.
 *
 * The columns are 1-based and counted in bytes, which is cppnix's own
 * convention (src/libexpr/lexer.l advances the column by the token's byte
 * length), so a line with a multibyte character reports the same column on
 * both arms.
 *
 * `unsigned int` and not `uint32_t`, matching what the rest of this header
 * spells a Rust `u32` (ixe_set_max_call_depth) -- abi_check.rs canonicalises
 * `u32` to exactly one C spelling, so the two are not interchangeable here
 * however identical they are to the compiler. */
typedef struct
{
    char * file;
    unsigned int line;
    unsigned int column;
} IxePos;

/* Who answers this session's questions about the outside world: a struct of
 * function pointers plus a context pointer, defined in full further down
 * beside the hook typedefs it is made of.
 *
 * Named and forward-declared here for the same reason IxeSession is: both
 * entry points below take one, and a type used before it is defined does not
 * compile. */
typedef struct IxeHostVtable IxeHostVtable;

/* 0 ok; 1 eval error; 2 unimplemented construct; 3 parse error; 4 bad call;
 * 5 builtins.throw (ThrownError); 6 failed assert (AssertionError); 8 import
 * from derivation disabled (IFDError). The exception classes are separate
 * because they cannot be read back out of the message. */
int ixe_eval_expr(
    /* who answers this evaluation's questions about the outside world, or
     * NULL for a host that answers none of them. Per call rather than
     * installed, for the reason on IxeHostVtable. */
    const IxeHostVtable * host,
    const unsigned char * src,
    size_t src_len,
    const unsigned char * base_dir, /* directory for relative paths; NULL = cwd */
    size_t base_dir_len,
    /* absolute path the source was read from, or NULL when it was not read
     * from one (--expr). This is what __curPos reports; cppnix answers null
     * for a string origin rather than naming a file, so NULL and a path are
     * different answers and not a default. ENG-12713. */
    const unsigned char * file,
    size_t file_len,
    char ** out,
    /* receives the refusal token when the status is IXE_ERR_UNIMPLEMENTED and
     * NULL otherwise, or may itself be NULL when the caller does not want it.
     * Static storage, like ixe_session_refusal_token: do not free it.
     *
     * Load-bearing rather than a convenience. This is the call
     * nix-instantiate --eval takes for a whole expression and the one the
     * result cache serves, and without a way to report the kind, every
     * refusal on the fleet's commonest path was counted as `unrecorded`
     * while the handle API's refusals carried their tokens fine. ENG-12819. */
    const char ** out_token,
    /* receives where the failure happened, or the "nowhere" value (line 0)
     * when it has none. May be NULL when the caller does not want it. Always
     * written before anything can fail, so a caller reading it after an early
     * return sees "nowhere" rather than whatever its own variable held. */
    IxePos * out_pos);
void ixe_string_free(char * s);

/**
 * This evaluation's perf counters, as one line of key=value pairs. The caller
 * owns the string and frees it with ixe_string_free; NULL on failure.
 *
 * The evaluator performs no IO, so it does not decide whether to print. It
 * accumulates and this hands the numbers over; the embedder chooses. None of
 * it is reachable from a Nix program or part of the memo key.
 *
 * `ops` is zero unless the crate was built with the `perf-ops` feature, and
 * `ops_counted` says which zero it is.
 */
char * ixe_perf_snapshot(void);

/**
 * Zero the counters. Call before an evaluation you intend to measure: without
 * it a second evaluation in one process reports the sum of both.
 */
void ixe_perf_reset(void);

/* Call-depth ceiling for subsequent ixe_eval_expr calls; mirrors cppnix's
 * max-call-depth. This VM holds its frames on the heap, so unbounded
 * recursion allocates rather than faulting and needs an explicit limit
 * (ENG-12432). */
void ixe_set_max_call_depth(unsigned int depth);

/* Directory for the on-disk cache of compiled modules and evaluation
 * results. NULL or zero length turns it off, which is the default and is
 * in-memory-only behaviour. Entries are content-addressed, so an edit misses
 * rather than needing invalidation, and removing the directory is always
 * safe. */
void ixe_set_eval_cache_dir(const unsigned char * path, size_t path_len);

/* How often that cache checks itself, as one occasion in this many. 0 is off
 * and the default; 1 checks everything. One hit in `rate` is re-evaluated and
 * compared against what was served -- the served answer is still what the
 * caller gets, so sampling cannot change an output -- and one record in
 * `rate` is looked up again in the same process, which is the half that sees
 * a cache writing rows it will never serve.
 *
 * A cache cannot be checked by reading its answers: its answers are by
 * construction whatever it was told to say. ENG-12541, a memo key blind to
 * the store directory, served paths for the wrong store and would have been
 * caught in production by a one-in-twenty check. Meaningless without
 * ixe_set_eval_cache_dir, since with no cache there is nothing to check. */
void ixe_set_cache_verify_rate(unsigned int rate);

/* Byte cap on that cache. After each result is published the store is swept,
 * least recently used entries first, until it fits; 0 (the default) never
 * sweeps and the cache grows without bound. The sweep reads directory
 * metadata and the small `.refs` sidecar beside each witness, never a witness
 * body, so it costs entries and not bytes. Eviction can only cause a later
 * miss, never a different answer. Meaningless without ixe_set_eval_cache_dir. */
void ixe_set_eval_cache_max_bytes(uint64_t bytes);

/* How the evaluator turns a path interpolated into a string into a store
 * path. cppnix coerces such a path with copyToStore set (eval.cc:2582), so
 * `"${./f}"` is the store path and not the source path, and only the
 * embedder owns a store to answer with. Without a hook the coercion reports
 * itself unimplemented rather than inventing a path (ENG-12447).
 *
 * `root` is empty for the ambient rootPath accessor. Otherwise it is the
 * exact storeFS mount point and `path` is relative to that mounted accessor.
 * The pair is parse-time provenance; the host must not resolve a spelling to
 * some other accessor.
 *
 * Return 0 and point *out at the store path, or non-zero and point it at the
 * error text. The callee keeps ownership and the buffer need only outlive the
 * call: the evaluator copies it before returning, so neither side frees
 * across the boundary and the two allocators never meet. */
typedef int (*ixe_copy_to_store_fn)(
    void * ctx,
    const unsigned char * root,
    size_t root_len,
    const unsigned char * path,
    size_t path_len,
    const unsigned char ** out,
    size_t * out_len);

/* builtins.storePath. The rooted input has the same provenance contract as
 * ixe_copy_to_store_fn. Apply cppnix's storePath rules, including its
 * symlink resolution, in-store check, lazy mounted-store exemption and
 * ensurePath call. On success write `store-object NUL visible-path` to *out;
 * the evaluator uses the first field as string context and returns the
 * second. Errors use the same status and buffer contract. */
typedef ixe_copy_to_store_fn ixe_store_path_fn;

/* How the embedder stores a text blob, for builtins.toFile. Same buffer
 * discipline as ixe_copy_to_store_fn. `references` is NUL-terminated fields: each
 * store path followed by a NUL, unambiguous because a store path cannot
 * contain one.
 *
 * The embedder owns the read-only decision: cppnix computes the path without
 * writing under settings.readOnlyMode and writes otherwise, and the evaluator
 * cannot see that setting. ENG-12607. */
typedef int (*ixe_store_text_fn)(
    void * ctx,
    const unsigned char * name,
    size_t name_len,
    const unsigned char * contents,
    size_t contents_len,
    const unsigned char * references,
    size_t references_len,
    const unsigned char ** out,
    size_t * out_len);

/* How the embedder copies a filtered tree into the store, for builtins.path.
 * Same buffer discipline as ixe_copy_to_store_fn.
 *
 * The filter is a Nix function, so the evaluator walks the tree itself and
 * applies it; what arrives here is the finished decision. `request` is
 * NUL-terminated fields:
 *
 *   1. the accessor root (empty for rootPath, otherwise the exact mount point);
 *   2. the path relative to that accessor;
 *   3. the store object's name;
 *   4. "nar" or "flat";
 *   5. the expected SHA-256 as SRI, or empty for "no sha256 attribute";
 *   6. "inherit-references" or "own-references";
 *   7. "unfiltered" (copy everything, cppnix's defaultPathFilter) or
 *      "filtered";
 *   8. when filtered, an accessor-relative path and a type ("regular", "directory", "symlink",
 *      "unknown") per accepted entry.
 *
 * "inherit-references" is addPath's store-path branch (primops.cc:2947): the
 * root coerced with a non-empty string context AND is already under the store
 * directory, so the copy takes the references of the store object the root
 * lives in. Do queryPathInfo(toStorePath(root).first)->references, put them in
 * the content address with makeFixedOutputPathFromCA, and use addToStore
 * rather than fetchToStore, which cannot carry references. The evaluator has
 * already realised the context and rewritten the root by the time this
 * arrives, so the query is against the built output. The references are part
 * of the content address: dropping them lands the copy on a well-formed WRONG
 * store path that then feeds a derivation hash, with nothing downstream able
 * to tell.
 *
 * The accepted list is exactly what cppnix's filter returned true for, and it
 * is closed downwards -- a directory absent from it has no descendants in it
 * -- so a membership test is a correct PathFilter. Do not re-decide it. Do
 * answer with the store path addPath would produce for the same name, method
 * and bytes; the evaluator gives the result that path as its string context.
 *
 * Without a hook builtins.path reports itself unimplemented rather than
 * answering with a path nobody archived. ENG-12678. */
typedef int (*ixe_store_filtered_fn)(
    void * ctx, const unsigned char * request, size_t request_len, const unsigned char ** out, size_t * out_len);

/* How the embedder registers derivations prepared by Rust.
 * The packet begins with a little-endian u64 derivation count, a u64 lazy
 * source count, and that many length-prefixed, eight-byte-padded source
 * paths. The remainder is the AddMultipleToStore stream: a count followed
 * by WorkerProto 1.16 ValidPathInfo records and NAR bytes.
 *
 * Rust owns canonical derivation parsing, references, paths, NAR framing,
 * hashes and metadata. The host materializes the listed lazy sources and
 * transports the prepared stream; it does not reinterpret derivations.
 *
 * A batch because the evaluator answers derivationStrict itself, from the
 * bytes, and tells the store later: at the first build, validity check,
 * read or toFile reference that names one of them, and at every return to
 * the embedder. cppnix pays one store round trip per derivationStrict (23k
 * of them for one NixOS closure); this pays one per batch.
 *
 * Leaving this NULL is not an error, and is not the same as leaving
 * store_text NULL. A derivation still evaluates and still reports the
 * drvPath it would have been written to; only the file is missing, which
 * is precisely cppnix under readOnlyMode. `nix build` needs the hook;
 * `nix eval` does not.
 *
 * Return 0 when every derivation in the batch is a store object with the
 * temporary root cppnix's writeDerivation gives one; *out may be empty.
 * Return non-zero with *out the failure text, under the same buffer
 * discipline as ixe_store_text_fn: the evaluation that wrote these
 * derivations fails, and the evaluator will not memoise it. */
typedef int (*ixe_write_drvs_fn)(
    void * ctx, const unsigned char * batch, size_t batch_len, const unsigned char ** out, size_t * out_len);

/* How the embedder fetches a URL into the store, for builtins.fetchurl and
 * builtins.fetchTarball. Same buffer discipline as ixe_copy_to_store_fn.
 * `request` is NUL-terminated fields:
 *
 *   1. the URL, already rewritten by resolvePseudoUrl for the tarball case;
 *   2. the store object's name, already defaulted (baseNameOf the URL, or
 *      "source" for fetchTarball) and already through checkName;
 *   3. "file" (ingest the bytes flat, fetchurl) or "tarball" (unpack and
 *      ingest as a NAR, fetchTarball);
 *   4. the expected SHA-256 as SRI, or empty for "no sha256 attribute".
 *
 * Do exactly what fetch() does from checkURI onward (primops/fetchTree.cc:462),
 * and in particular keep its early exit: with a sha256, compute the
 * fixed-output path and ensurePath it, and answer with that path if the store
 * can produce it WITHOUT downloading. That branch is what makes a pinned
 * evaluation hermetic, and it has to live here because whether the store holds
 * a path is a fact only the store knows.
 *
 * Without a hook the fetchers report themselves unimplemented rather than
 * answering with a path nobody downloaded. */
typedef int (*ixe_fetch_fn)(
    void * ctx, const unsigned char * request, size_t request_len, const unsigned char ** out, size_t * out_len);

/* How the embedder fetches a tree, for builtins.fetchTree and
 * builtins.fetchGit. Same buffer discipline as ixe_copy_to_store_fn.
 * `request` is NUL-terminated fields:
 *
 *   1. the fetcher, "fetchTree" or "fetchGit";
 *   2. then a name, a one-letter type tag ("s" string, "b" Boolean as 0/1,
 *      "i" non-negative integer) and a value, per input attribute, in name
 *      order.
 *
 * The evaluator has forced and classified the attributes and raised the
 * errors a program can see; everything about building and fetching the Input
 * is yours. In particular DO apply fixGitURL, the exportIgnore and shallow
 * defaults, the registry lookup, the pure-eval locked-input check and the
 * input cache -- the evaluator deliberately does none of them, which is why
 * the fetcher name is in the request.
 *
 * The answer is NOT a store path: it is the JSON of the attribute set
 * emitTreeAttrs builds (outPath, narHash, rev, revCount, lastModified, a
 * nested history, whatever the input type has). The evaluator gives outPath
 * its own path as string context, which JSON cannot carry.
 *
 * Return 0 with the JSON, or 2 with a message meaning "this embedder will not
 * serve this request" -- which the evaluator reports as a NAMED REFUSAL and
 * never as an evaluation error, so a census counts it as a gap rather than a
 * wrong answer. Any other non-zero is an ordinary failure with an error
 * message. That third outcome exists because a tree fetch under the read-set
 * tracker cannot be served: emitTreeAttrs returns per-attribute recording
 * thunks and this boundary cannot carry them.
 *
 * Without a hook the tree fetchers report themselves unimplemented. */
typedef int (*ixe_fetch_tree_fn)(
    void * ctx, const unsigned char * request, size_t request_len, const unsigned char ** out, size_t * out_len);

/* How the evaluator locks a flake, for builtins.getFlake.
 *
 * cppnix's prim_getFlake is two halves: lockFlake, then callFlake. This hook
 * is the first half. The evaluator sends the flake reference exactly as the
 * program wrote it -- already forced to a context-free string, as
 * forceStringNoCtx does, and not otherwise touched. Parsing it, the pure-eval
 * rule that refuses an unlocked reference, the registry, the input-graph walk
 * and every fetch are the embedder's, and their errors should be cppnix's own
 * words rather than the backend's.
 *
 * The answer is one JSON object with three STRING fields:
 *
 *   source     call-flake.nix, verbatim. Sent rather than embedded in the
 *              evaluator so the two backends run one copy of the 105-line
 *              program that decides which tree every input resolves to.
 *   lockFile   the lock file, as the text call-flake.nix calls fromJSON on.
 *   overrides  the overrides document, itself a JSON document carried as a
 *              string. A string and not a nested object on purpose: a read set
 *              digests these bytes, and re-parsing plus re-serialising would
 *              put key ordering between the bytes produced and the bytes
 *              digested.
 *
 * The third argument callFlake applies -- fetchTreeFinal -- is NOT here. It is
 * a function, which this boundary cannot carry, and the evaluator already has
 * it as its own fetchFinalTree builtin.
 *
 * Return 0 with the JSON, or 2 with a message meaning "this embedder will not
 * serve this" -- reported as a NAMED REFUSAL and never as an evaluation error,
 * so a census counts it as a gap rather than a wrong answer. Any other
 * non-zero is an ordinary failure with an error message. The third outcome
 * exists because locking under the read-set tracker cannot be served:
 * emitTreeAttrs returns per-attribute recording thunks and the overrides
 * document forces every one of them.
 *
 * Without a hook builtins.getFlake reports itself unimplemented rather than
 * inventing a lock, which would name trees nobody fetched. */
typedef int (*ixe_lock_flake_fn)(
    void * ctx, const unsigned char * flake_ref, size_t flake_ref_len, const unsigned char ** out, size_t * out_len);

/* How the embedder parses a flake reference: builtins.parseFlakeRef.
 *
 * The request is the reference string, already forced context-free. The
 * answer is one JSON object of string, integer and Boolean fields -- run
 * fetchers::attrsToJSON over parseFlakeRef(...).toAttrs(), which are the
 * three shapes fetchers::Attr holds. The grammar is the embedder's on
 * purpose: URL schemes, path refs, indirect refs, and a second parser in the
 * evaluator would be a second set of attrs for one string to explode to.
 *
 * The flakes feature gate belongs behind this hook, where cppnix checks it:
 * the primop is registered unconditionally and the CALL raises the
 * feature-is-disabled error, so the hook should require the feature first
 * and let that error travel back as an ordinary failure.
 *
 * Same three-outcome contract and buffer discipline as ixe_lock_flake_fn.
 * Without a hook builtins.parseFlakeRef reports itself unimplemented. */
typedef int (*ixe_parse_flake_ref_fn)(
    void * ctx, const unsigned char * flake_ref, size_t flake_ref_len, const unsigned char ** out, size_t * out_len);

/* How the embedder prints a flake reference: builtins.flakeRefToString.
 *
 * The request is a name, a one-letter type tag ("s", "b", "i") and a value
 * per attribute, NUL-terminated in name order -- ixe_fetch_tree_fn's
 * encoding without its leading fetcher field. Build fetchers::Attrs from the
 * triplets and answer FlakeRef::fromAttrs(...).to_string(). The evaluator
 * has already raised the negative-integer and wrong-type errors on its side.
 *
 * Same contract and feature gate as ixe_parse_flake_ref_fn. */
typedef int (*ixe_flake_ref_to_string_fn)(
    void * ctx, const unsigned char * request, size_t request_len, const unsigned char ** out, size_t * out_len);

/* How the evaluator makes a store path present. builtins.appendContext
 * validates each key it is handed with isStorePath and then calls
 * ensurePath (context.cc:270), which substitutes or builds the path when it
 * is missing -- and skips it entirely under readOnlyMode. Both the store and
 * that setting are the embedder's, so the branch belongs on its side of this
 * hook, not in the evaluator. Without a hook appendContext reports itself
 * unimplemented rather than admitting an unvalidated key (ENG-12479).
 *
 * Return 0 for present. On non-zero, point *out at the error text under the
 * same buffer contract as ixe_copy_to_store_fn. */
typedef int (*ixe_ensure_path_fn)(
    void * ctx, const unsigned char * path, size_t path_len, const unsigned char ** out, size_t * out_len);

/* Which of a list of store paths the store holds now: Store::queryValidPaths,
 * one round trip for the whole list. `paths` is newline-separated printed
 * store paths; on 0, *out is the newline-separated subset that is valid
 * (possibly empty). Asked by the evaluation-cache verifier for the objects a
 * witness relies on -- the derivations it wrote, the outputs it realised,
 * the copies and fetches it made -- so that a hit checks each is still there
 * instead of writing, building or copying it again. Which objects a row
 * relies on is the verifier's business; this says only what the store holds.
 * On non-zero, *out is the error text under the same buffer contract as
 * ixe_copy_to_store_fn. */
typedef int (*ixe_valid_paths_fn)(
    void * ctx, const unsigned char * paths, size_t paths_len, const unsigned char ** out, size_t * out_len);

/* Which of a list of store objects are sealed -- held by the store and
 * content-addressed (the path is the one its content address derives), so
 * the name pins every byte, symlink text included -- and, for each, every
 * symlink under it. `paths` is newline-separated "<hash>-<name>" lines; on
 * 0, *out has one line per sealed object: the name verbatim, then for each
 * symlink its object-relative path and its target, all tab-separated (an
 * even number of fields after the name; possibly no lines; an object the
 * store does not hold is simply absent). A path or target that a line
 * cannot carry (a control character) leaves its object out of the answer.
 * Which links leave the object -- and so which reads under it must ask
 * rather than replay -- the evaluator decides (readset::leaving_links).
 * Asked by the evaluation-cache verifier once per object it has not yet
 * recorded, which it then never asks again: content addressing and the link
 * graph are permanent facts about immutable bytes; presence is checked
 * every time through ixe_valid_paths_fn. On non-zero, *out is the error text
 * under the same buffer contract as ixe_copy_to_store_fn. */
typedef int (*ixe_sealed_paths_fn)(
    void * ctx, const unsigned char * paths, size_t paths_len, const unsigned char ** out, size_t * out_len);

/* `allowPath` for each NUL-terminated store path in `paths`: what the copy,
 * fetch and tree hooks end with, for an effect the evaluation-cache verifier
 * served by validity instead of re-running. Not the closure form: a copy
 * allows its own path and nothing it references; realised outputs go
 * through ixe_realise_allow_fn. On non-zero, *out is the error text under
 * the same buffer contract as ixe_copy_to_store_fn. */
typedef int (*ixe_allow_paths_fn)(
    void * ctx, const unsigned char * paths, size_t paths_len, const unsigned char ** out, size_t * out_len);

/* How the embedder realises a string context: import from derivation.
 *
 * This is EvalState::realiseContext (primops.cc:72) split into three hooks,
 * so the evaluator can run the build off its own thread and keep evaluating
 * (ENG-13150). Whenever a read-shaped builtin -- import, readFile, readDir,
 * pathExists, findFile, builtins.path, filterSource -- coerces a path whose
 * string context is not empty, the evaluator asks this FIRST and only then
 * asks its read question, which is the order realisePath (primops.cc:167)
 * uses. Supply all three or none; a partial set is refused at session
 * creation, because the phases only mean anything as a protocol. Without
 * them, a read through a derivation output refuses by name --
 * StoreUnavailable -- rather than reading a path nothing built.
 *
 * `request`, for phases 1 and 2, is the context: NUL-terminated fields, one
 * per element, each rendered as NixStringContextElem::to_string renders it
 * -- "!<output>!<drvpath>" for a single output, "=<drvpath>" for a deep
 * dependency, or a bare store path for an opaque one. Parse them back with
 * NixStringContextElem::parse; do not invent a second spelling. A NUL cannot
 * occur in a store path, so the framing is unambiguous, and an empty request
 * is never sent.
 *
 * Everything policy-shaped is the embedder's, because none of it is visible
 * from inside the evaluator: the isValidPath check on each element (an
 * InvalidPathError, program-visible), allow-import-from-derivation and its
 * IFDError, trace-import-from-derivation, buildPaths, and copyClosure plus
 * allowClosure when the build store is not the evaluation store.
 *
 * The protocol, for one realise question. The evaluator's blocking route
 * runs phase 1 on the calling thread, queues phase 2 on the session's build
 * dispatcher, then runs phase 3 when the answer is delivered. The blocking
 * route waits for the same dispatcher. Each phase runs at most once per
 * question. Session destruction joins the dispatcher before returning.
 *
 *   1. realise_check(request)   evaluation thread. Everything realiseContext
 *                               does before building that touches the
 *                               embedder's evaluation-side state: validity
 *                               checks (and their read-set recording), the
 *                               allow-import-from-derivation refusal, the
 *                               trace warning. The status is an
 *                               IxeRealiseCheck: BUILD when something needs
 *                               building, NOTHING when every element is
 *                               already valid (the answer is then the empty
 *                               rewrite map and no other phase runs), or
 *                               FAILED with the message under the same
 *                               buffer contract as ixe_copy_to_store_fn and
 *                               `error_class` set. The class preserves
 *                               disabled IFD as its own exception class;
 *                               other failures become an uncatchable
 *                               evaluation error. Both match cppnix's
 *                               behaviour: prim_tryEval catches
 *                               AssertionError alone (primops.cc:1219).
 *
 *   2. realise_build(requests)  A WORKER THREAD THE EVALUATOR OWNS, the one hook in this
 *                               vtable with that contract, and supplying it
 *                               is the embedder's written consent. It may
 *                               run concurrently with every other hook
 *                               (called on the evaluation thread). Build
 *                               calls within one session never overlap.
 *                               It must therefore touch only
 *                               state that serves concurrent callers -- for
 *                               the nix embedder that is the stores -- and
 *                               its answer buffer must not be shared with
 *                               any other hook or call: thread-local is the
 *                               natural shape. Each request has the bytes
 *                               check saw; the buffers stay live until this
 *                               call returns, as everywhere else in this
 *                               ABI.
 *
 *                               Success (0) writes the rewrite map
 *                               realiseContext returns -- NUL-terminated
 *                               fields, an even number, alternating from
 *                               and to; under ca-derivations the
 *                               DownstreamPlaceholder -> real-output-path
 *                               map the evaluator rewrites the path it is
 *                               about to read with -- then ONE EMPTY FIELD
 *                               as a separator, then the built output store
 *                               paths, one per field, all NUL-terminated.
 *                               Neither a placeholder nor a store path can
 *                               be empty, so the separator is unambiguous.
 *                               Failure is non-zero with the message; it
 *                               reaches the program as an uncatchable
 *                               evaluation error.
 *
 *   3. realise_allow(outputs)   evaluation thread, at the moment the answer
 *                               is delivered -- which the scheduler orders
 *                               by ask order, not completion order. The
 *                               outputs section of realise_build's answer,
 *                               verbatim. Register them (and their
 *                               closures) in the evaluator-side access
 *                               allow list. This exists because that list
 *                               is single-threaded state the evaluation
 *                               thread reads on every file access: the
 *                               build must never mutate it from the worker.
 *                               Runs before the program can see the
 *                               answer, so no read through a built output
 *                               can precede its registration. */
typedef enum IxeRealiseErrorClass {
    IXE_REALISE_ERROR_OTHER = 0,
    IXE_REALISE_ERROR_IMPORT_FROM_DERIVATION = 1,
} IxeRealiseErrorClass;

typedef enum IxeRealiseCheck {
    IXE_REALISE_CHECK_BUILD = 0,
    IXE_REALISE_CHECK_FAILED = 1,
    IXE_REALISE_CHECK_NOTHING = 2,
} IxeRealiseCheck;

typedef int (*ixe_realise_check_fn)(
    void * ctx,
    const unsigned char * request,
    size_t request_len,
    /* One of the IxeRealiseErrorClass values, written as a plain int on
       IXE_REALISE_CHECK_FAILED: the evaluator decodes it and refuses any
       value the enum does not name, rather than holding an enum cell C may
       have filled with anything. */
    int * error_class,
    const unsigned char ** out,
    size_t * out_len);
typedef struct {
    const unsigned char * data;
    size_t len;
} IxeRealiseRequest;

typedef struct {
    int status;
    const unsigned char * data;
    size_t len;
} IxeRealiseResult;

/* The dispatcher calls one batch at a time. Requests and result slots have
 * count elements. Fill every result slot, preserving request order. A result
 * has status 0 and the existing build-answer encoding, or a nonzero status
 * and its error message. Result bytes remain valid until the next build call.
 * Return nonzero only when the entire callback failed to produce results.
 * The host must keep independent requests alive when one build fails. */
typedef int (*ixe_realise_build_fn)(
    void * ctx, const IxeRealiseRequest * requests, size_t count, IxeRealiseResult * results);
typedef int (*ixe_realise_allow_fn)(
    void * ctx, const unsigned char * outputs, size_t outputs_len, const unsigned char ** out, size_t * out_len);

/* Where a warning from the evaluator goes. cppnix warns about six derivation
 * attributes that __structuredAttrs silently disables (primops.cc:1693); a
 * backend that stayed quiet would be telling the reader less than cppnix
 * does. The logger and its verbosity are the embedder's, so the message is
 * handed over rather than printed here. The callee must not retain the
 * buffer, which is only valid for the call. */
typedef void (*ixe_warn_fn)(void * ctx, const unsigned char * message, size_t message_len);

/* How a search path is resolved: `<nixpkgs>` and builtins.findFile.
 *
 * The evaluator cannot do this itself. cppnix's EvalState::findFile resolves
 * each entry through resolveLookupPathPath, which downloads a pseudo-URL into
 * the store, consults the registered lookup-path hooks, resolves symlinks and
 * applies the evaluator's access control, and falls back to the in-memory
 * corepkgs accessor for a name starting "nix/". ENG-12443.
 *
 * `entries` is the list to walk, NUL-separated and in pairs: prefix, path,
 * prefix, path, ... with a trailing NUL after each field, so an empty prefix
 * is an empty field and not an omission. `entries_len` counts every byte
 * including those NULs. The list travels with the question because it is an
 * ordinary Nix value the program can rebind, not a process setting.
 *
 * On success, write `root NUL accessor-relative-path` to *out. An empty root
 * selects the ambient rootFS accessor. A non-empty root is the exact complete
 * store path used as a key in cppnix's storeFS mount table; the second field
 * starts with `/` and is relative to that mounted accessor. Both fields must
 * be canonical. Return 5 with error text for "not found", which cppnix raises
 * as a ThrownError and builtins.tryEval therefore catches; return 2 with
 * error text when the name resolved to something this embedding cannot hand
 * the evaluator at all (cppnix's in-memory corepkgs beyond fetchurl.nix),
 * which the evaluator reports as its own unimplemented construct rather than
 * as a mismatch; return 1 with error text for anything else. A status 0 whose
 * payload does not decode is a protocol fault and is reported as a failure,
 * never as a refusal. Same buffer contract as ixe_copy_to_store_fn.
 *
 * A name that resolves into an accessor the evaluator cannot read -- cppnix's
 * in-memory corepkgs, a downloaded search-path entry behind its fetcher --
 * is answered with a THIRD field: `NUL accessor-relative-path NUL contents`,
 * the empty root selecting the ambient accessor. The evaluator holds those
 * bytes for the session and serves every later question about the path from
 * them (existence, kind, contents, import), which is how
 * `builtins.toString <nix/fetchurl.nix>` stays `/fetchurl.nix` on both arms.
 * Contents beside a non-empty root are a protocol fault: a mounted root is a
 * real accessor. */
typedef int (*ixe_find_file_fn)(
    void * ctx,
    const unsigned char * entries,
    size_t entries_len,
    const unsigned char * name,
    size_t name_len,
    const unsigned char ** out,
    size_t * out_len);

/* The default search path, which is builtins.nixPath and the second argument
 * every `<x>` desugars to. Same NUL-separated pair encoding as the `entries`
 * argument above, written to *out.
 *
 * Asked rather than pushed at setup because it changes what an expression
 * evaluates to, so a read set has to see it: a memoised result must miss when
 * -I changes. Return 0 on success, non-zero with error text in *out. */
typedef int (*ixe_nix_path_fn)(void * ctx, const unsigned char ** out, size_t * out_len);

/* The plain filesystem reads: builtins.readFile, both pathExists shapes,
 * readDir and readFileType, plus the resolving kind query an `import` is half
 * made of.
 *
 * Carried as a set by IxeHostVtable below, and read through the embedder for
 * one reason: pure-eval and restrict-eval are enforced in cppnix
 * by wrapping EvalState::rootFS in an AllowListSourceAccessor (eval.cc:306),
 * so a read that does not go through that accessor cannot honour either
 * setting. Without these hooks the evaluator reads with std::fs, and
 * rust/nix-eval-rs/src/purity.rs refuses all seven questions under either
 * setting rather than answering them outside the allow list. Supplying them
 * is what makes a flake evaluable, because flake entry means importing files
 * out of a fetched store path under pure eval. ENG-12792.
 *
 * Do what the cppnix primop that asks the same question does, INCLUDING its
 * realisePath call and not merely what follows it. The symlink resolution is
 * per primop and they genuinely differ:
 *
 *   prim_readFile      primops.cc:2203  realisePath(pos, *args[0])
 *                                       -> SymlinkResolution::Full (the
 *                                          default argument, eval.hh:1133)
 *   prim_pathExists    primops.cc:2092  Ancestors for the plain callback;
 *                                       a string ending "/" or "/." selects
 *                                       the separate directory-existence
 *                                       callback before coercion, and that
 *                                       callback resolves Full
 *   prim_readDir       primops.cc:2510  realisePath(pos, *args[0]) -> Full
 *   prim_readFileType  primops.cc:2492  realisePath(pos, *args[0], nullopt)
 *                                       -> resolves NOTHING, so a symlinked
 *                                          ancestor raises SymlinkNotAllowed
 *                                          rather than answering
 *   import             primops.cc:300   realisePath(pos, vPath, nullopt), and
 *                                       then resolveExprPath (eval.cc:3423)
 *                                       does its own resolution: its
 *                                       directory test is
 *                                       path.resolveSymlinks().lstat().type
 *                                       (eval.cc:3440), ie Full
 *
 * That is why the kind query is two hooks rather than one. Skipping the
 * resolution entirely was ENG-12871: `builtins.readDir ./link-to-dir` and
 * `import ./a/symlinked-dir/f.nix` both raised "is a symlink" on this backend
 * and both evaluate on cppnix, because PosixSourceAccessor deliberately
 * refuses rather than follows (posix-source-accessor.cc:198) -- cppnix puts
 * the resolution in EvalState::realisePath and not in the accessor.
 *
 * Keep prim_pathExists's narrow catch: a forbidden path is `false` there and
 * must be `false` here. Missing is also false. Other failures propagate.
 *
 * Same buffer discipline as ixe_copy_to_store_fn: return 0 and point *out at
 * the answer, or non-zero and point it at the error text, which the evaluator
 * reports as an ordinary evaluation error. The two existence hooks'
 * exception contract is documented separately below. */
typedef int (*ixe_read_file_fn)(
    void * ctx,
    const unsigned char * root,
    size_t root_len,
    const unsigned char * path,
    size_t path_len,
    const unsigned char ** out,
    size_t * out_len);

/* Resolve and read an import in one accessor-stable operation. On success
 * the answer is root NUL accessor-relative-path NUL source-bytes. The source
 * occupies the remainder of the buffer and may itself contain NUL bytes. */
typedef ixe_read_file_fn ixe_import_fn;

/* Whether a path exists. On success return 0 and write `1` or `0` to *out.
 * A missing path and RestrictedPathError answer `0`. Every other failure,
 * including a vanished mounted root or an I/O error, returns non-zero with
 * error text. This distinction is part of replay correctness: an error must
 * not digest as the same observation as a missing file. */
typedef ixe_read_file_fn ixe_path_exists_fn;

/* The value-level trailing-slash branch of builtins.pathExists. On success,
 * write `1` only when full symlink resolution ends at a directory, otherwise
 * `0`. Its three outcomes have the same contract as ixe_path_exists_fn:
 * missing and RestrictedPathError are false; every other failure is an
 * error. Kept separate because a canonical path no longer carries the
 * trailing slash that selected this operation. It uses the
 * ixe_path_exists_fn signature in the vtable. */

/* A directory listing, written to *out as NUL-terminated fields in pairs:
 * name, type, name, type, ... with a trailing NUL after each field, so every
 * pair is two fields and a partial pair is a malformed answer rather than an
 * entry with a missing half. `out_len` counts every byte including those NULs.
 *
 * The type is one of "regular", "directory", "symlink", "unknown", cppnix's
 * own four spellings, which are what builtins.readDir returns
 * (primops.cc:2480). A fifth spelling is rejected rather than folded into
 * "unknown", because "unknown" is a real answer and swallowing a typo into it
 * would turn a broken hook into a plausible directory listing.
 *
 * cppnix's readDirectory can report an entry whose type the filesystem did
 * not give, which prim_readDir turns into a lazy builtins.readFileType call.
 * There is no lazy field here, so resolve it before answering. */
typedef int (*ixe_read_dir_fn)(
    void * ctx,
    const unsigned char * root,
    size_t root_len,
    const unsigned char * path,
    size_t path_len,
    const unsigned char ** out,
    size_t * out_len);

/* What a path is, written to *out as one of the four spellings above.
 *
 * The vtable has two fields of this shape and they must not hold the same
 * function. builtins.readFileType is the lstat one and resolves nothing;
 * import's directory test is the stat one and resolves everything. See the
 * per-primop table above.
 *
 * The non-resolving one -- and only that one -- may also answer the fifth
 * spelling "absent", with return 0, for a path the accessor has no answer
 * for. That is SourceAccessor::maybeLstat returning nullopt, which is the
 * primitive: SourceAccessor::lstat is maybeLstat plus throw FileNotFound
 * (source-accessor.cc:73). Hand the nullopt over rather than the throw,
 * because two callers ask this question and want opposite things from a
 * missing path -- builtins.readFileType wants cppnix's error, and the
 * ancestor scan under a filtered builtins.path wants what cppnix's
 * resolveSymlinks (source-accessor.cc:91) gets from the same call: "not a
 * symlink", recorded as the observation "absent". The evaluator raises the
 * error for the caller that wants it, so which one gets it stays a semantic
 * decision on the evaluator's side of this boundary.
 *
 * Returning non-zero for a missing path instead is not a compile error and
 * not an ABI break; it is the old behaviour, and it costs every filtered
 * builtins.path under pure eval, where rootFS is a mounted accessor knowing
 * only /nix/store and so /nix -- an ancestor of every store path -- reads as
 * missing (ENG-13123).
 *
 * "absent" is the only answer that moves. A forbidden path, a symlinked
 * ancestor under the lstat hook, or a failed read is still non-zero with its
 * text: those are the accessor refusing or failing rather than answering, and
 * folding one into "absent" reports a forbidden path as an ordinary missing
 * one. The resolving hook has no "absent": its caller is cppnix's lstat.
 * ixe_read_dir_fn has none either -- an entry a directory listed and the
 * accessor cannot see is a broken hook, not a file with no type. */
typedef int (*ixe_file_type_fn)(
    void * ctx,
    const unsigned char * root,
    size_t root_len,
    const unsigned char * path,
    size_t path_len,
    const unsigned char ** out,
    size_t * out_len);

/* The seven are all present or all absent, and a vtable with some of them is
 * REFUSED rather than partly honoured. purity.rs decides those questions as a
 * group: the settings can be honoured exactly when every one of the reads
 * goes through an accessor that applies the allow list, so "six of seven" is
 * a state the evaluator would have to call both honoured and not. All seven
 * NULL is the standalone embedding -- the evaluator reads with std::fs and
 * refuses all seven questions under either purity setting. */

/* Where a builtins.trace line goes. Separate from ixe_warn_fn because cppnix
 * sends the two to different places: warn builds an ErrorInfo at lvlWarn,
 * trace calls printError with its own "trace: " prefix (primops.cc:1325), and
 * the prefix belongs on the cppnix side so there is one copy of the wording.
 * Same buffer rules as ixe_warn_fn: valid for the call only. */
typedef void (*ixe_trace_fn)(void * ctx, const unsigned char * message, size_t message_len);

/* Whether the embedder wants the running evaluation to stop. Called
 * periodically from the VM's poll loop; return non-zero to make the
 * evaluation fail with cppnix's own "interrupted by the user" wording.
 *
 * A deliberate divergence from cppnix, which checks no interrupt during
 * evaluation at all and so cannot kill a runaway one either (ENG-12533). The
 * hook must not throw, block, or call back into the evaluator: it runs inside
 * the VM rather than at a scheduler boundary, and nix::isInterrupted() -- an
 * atomic load -- is the shape it is meant for. */
typedef int (*ixe_interrupted_fn)(void * ctx);

/* Everything above, as one value: who answers this session's questions.
 *
 * Passed to ixe_session_new and to ixe_eval_expr, copied by the callee, and
 * never installed anywhere a second session can see. That is the whole point.
 * These used to be process-global slots with ixe_set_* setters, so two
 * sessions in one process shared one set of answers, whoever set them last
 * won, and a setter racing an evaluation could change what a running one saw.
 * Guarding that was three separate changes; making the host an argument
 * deletes the class.
 *
 * Every field may be NULL, and NULL means the evaluator answers for itself:
 * an operation with no hook reports itself unimplemented (a store or fetch
 * question), resolves nothing (find_file, nix_path), does nothing (warn,
 * trace, interrupted), or reads with std::fs (the seven path reads). The one
 * combination that is refused rather than defaulted is a PARTIAL set of the
 * seven reads; see above.
 *
 * `ctx` is handed back to every function unchanged and is the embedder's
 * place for per-session state -- an EvalState, a store handle, the buffers
 * an answer is written into. The struct itself need only outlive the call it
 * is passed to; everything it points at, `ctx` included, must outlive the
 * session. */
struct IxeHostVtable
{
    void * ctx;
    ixe_copy_to_store_fn copy_to_store;
    ixe_store_path_fn store_path;
    ixe_store_text_fn store_text;
    ixe_write_drvs_fn write_derivations;
    ixe_store_filtered_fn store_filtered;
    ixe_fetch_fn fetch;
    ixe_fetch_tree_fn fetch_tree;
    ixe_lock_flake_fn lock_flake;
    ixe_parse_flake_ref_fn parse_flake_ref;
    ixe_flake_ref_to_string_fn flake_ref_to_string;
    ixe_ensure_path_fn ensure_path;
    ixe_valid_paths_fn valid_paths;
    ixe_sealed_paths_fn sealed_paths;
    ixe_allow_paths_fn allow_paths;
    /* All three or none; see the protocol above ixe_realise_check_fn. */
    ixe_realise_check_fn realise_check;
    ixe_realise_build_fn realise_build;
    ixe_realise_allow_fn realise_allow;
    ixe_find_file_fn find_file;
    ixe_nix_path_fn nix_path;
    ixe_warn_fn warn;
    ixe_trace_fn trace;
    ixe_interrupted_fn interrupted;
    /* All seven or none; see the note above ixe_read_file_fn. */
    ixe_import_fn import_source;
    ixe_read_file_fn read_file;
    ixe_path_exists_fn path_exists;
    ixe_path_exists_fn dir_exists;
    ixe_read_dir_fn read_dir;
    ixe_file_type_fn file_type;
    ixe_file_type_fn file_type_resolved;
};

/* The two purity settings, passed separately because they forbid different
 * things. Non-zero for on. Set both from EvalSettings before evaluating.
 *
 * These replace one ixe_set_filesystem_access(restrictEval || pureEval) call,
 * which refused every host question whenever either was on. That was wrong in
 * the direction that matters: a flake is evaluated under pure eval by
 * default, so the wholesale refusal made no flake evaluable on this backend.
 *
 * What each setting forbids per question is in rust/nix-eval-rs/src/purity.rs,
 * one row per question with the cppnix line each was read off. The line the
 * table draws: a question the embedder answers through cppnix's own rootFS or
 * checkURI is served under both settings, because cppnix's access control
 * already applies and its own error text comes back. A question this crate's
 * own Host answers with a direct std::fs read -- import, readFile, both
 * pathExists shapes, readDir, and the two path-kind queries -- is refused,
 * because that Host consults no allow list and cannot tell an allowed path
 * from a forbidden one.
 *
 * That claim about the Rust side's Host is held by section 8b of
 * maintainers/ix/rust-nix-eval-gate.sh, which reads a file under each setting
 * and requires a named refusal, rather than by this comment. When ENG-12480
 * routes those seven questions through the accessor too, that section is what
 * should fail and the last Refuse row in purity.rs goes away with it. */
void ixe_set_pure_eval(int on);
void ixe_set_restrict_eval(int on);

/* Whether builtins.traceVerbose traces, from cppnix's `trace-verbose`.

 * Not a hook and not presentation. cppnix chooses between two different
 * primops with this setting (primops.cc:5560), and the one it picks when the
 * setting is off is `prim_second`, which never forces the message -- so
 * `builtins.traceVerbose (throw "x") 1` answers 1 with it off and dies with
 * it on. It is therefore in the evaluator's memo key, unlike the vtable's
 * `trace`, which only says where a line goes. */
void ixe_set_trace_verbose(int on);

/* Whether builtins.warn aborts after warning, from cppnix's `abort-on-warn`.

 * Also a value-deciding setting rather than a sink: with it on, an expression
 * that warns has no value at all (primops.cc:1369). Left unforwarded, this
 * evaluator would answer where cppnix dies. Also in the memo key. */
void ixe_set_abort_on_warn(int on);

/* Whether cppnix's `ca-derivations` experimental feature is enabled.

 * Value-deciding like the two above: with it off, `__contentAddressed = true`
 * is the feature-is-disabled error (primops.cc:1632), and with it on the
 * same derivation is a floating-CA `.drv`. Also in the memo key. */
void ixe_set_ca_derivations(int on);

/* Whether cppnix's `blake3-hashes` experimental feature is enabled.

 * Value-deciding wherever a hash algorithm is parsed: with it off Blake3 is
 * the feature-is-disabled error, and with it on the same expression computes
 * a 32-byte digest (libutil/hash.cc:25-29,468-473). Also in the memo key. */
void ixe_set_blake3_hashes(int on);

/* Whether `allow-import-from-derivation` is on. In the memo key: a witness
 * recorded with it on holds realisations the verifier serves by validity
 * without reaching `realiseContextCheck`, which refuses them with it off. */
void ixe_set_allow_import_from_derivation(int on);

/* `allowed-uris`, one entry per newline-terminated line. In the memo key: a
 * fetch served by validity never reaches `checkURI`, so a witness recorded
 * under a wider list must not hit under a narrower one. */
void ixe_set_allowed_uris(const unsigned char * uris, size_t uris_len);

/* Whether the evaluation runs under `--repair`. Nothing is served from the
 * memo under repair: repair means redo, and a served answer would skip the
 * rewrites `writeDerivation(repair)` performs on valid derivations. */
void ixe_set_repair(int on);

/* cppnix's three parser lints, as levels: 0 ignore, 1 warn, 2 fatal.
 *
 * Value-deciding at `fatal` only: cppnix's parser then rejects the linted
 * literal (parser.y:372-466), so the compiler on this side rejects the same
 * text. At `warn` cppnix prints a diagnostic this backend does not -- that
 * is tier-2 warning text, the line drawn when the bridge refused `fatal`
 * outright instead of forwarding it (ENG-12569, ENG-12597). Fatal-ness is
 * in the memo key. `lint-absolute-path-literals` covers `~/x` home literals
 * too, as cppnix's HPATH rule does. */
void ixe_set_lint_url_literals(int level);
void ixe_set_lint_short_path_literals(int level);
void ixe_set_lint_absolute_path_literals(int level);

/* Whether cppnix's `pipe-operators` experimental feature is enabled.

 * Value-deciding at parse time: with it off `a |> f` is the
 * feature-is-disabled error (lexer.l:89-96), and with it on the same text is
 * `f a` (parser.y:287-295). Also in the memo key. */
void ixe_set_pipe_operators(int on);

/* Whether cppnix's `parse-toml-timestamps` experimental feature is enabled.

 * Value-deciding: with it off a TOML date or time is `error: while parsing
 * TOML: Dates and times are not supported`, and with it on the same document
 * evaluates to `{ _type = "timestamp"; value = "..."; }` sets (primops.cc,
 * prim_fromTOML). Also in the memo key. */
void ixe_set_parse_toml_timestamps(int on);

/* Explicit language capabilities. Flakes also enables fetchTree. Unknown bits
 * return IXE_ERR_BADCALL without changing settings. Captured in every memo key. */
#define IXE_BUILTIN_FLAKES 1u
#define IXE_BUILTIN_FETCH_TREE 2u
#define IXE_BUILTIN_WASM 4u
int ixe_set_builtin_features(unsigned int flags);

/* Owned language documentation JSON, independent of any store/session. Both
 * output slots must be distinct and non-null; free non-null strings with
 * ixe_string_free. On success *error is null. */
int ixe_language_docs(char ** out, char ** error);

/* The stable name for why the last call refused, or NULL when the last
 * failure was not a refusal (IXE_ERR_UNIMPLEMENTED).
 *
 * Static storage: do NOT free it, and it stays valid for the life of the
 * process, so it can be used as a map key without copying. Reading it does
 * not clear it, unlike ixe_session_take_error, so the two can be read in
 * either order.
 *
 * The message from ixe_session_take_error is prose for a human and is free to
 * be reworded; this is what a census groups by. Counting refusals by slicing
 * the message made two refusals of one kind look like two whenever they
 * interpolated different names, and made rewording an error reset the
 * population silently. */
const char * ixe_session_refusal_token(IxeSession * session);

/* The three settings below are fixed for the lifetime of the process, and
 * each returns 0 on success or 4 (IXE_ERR_BADCALL) when the caller is trying
 * to *change* one that is already set. Repeating the same value is the
 * expected case and succeeds. On a refusal, ixe_take_setting_conflict()
 * returns a malloc'd sentence naming both values; free it with
 * ixe_string_free.
 *
 * They refuse rather than ignore because ignoring is what they used to do:
 * an embedder serving a second store kept the first store's directory and
 * computed every path under it, silently, with results memoised under a key
 * that claimed otherwise (ENG-12541). */
char * ixe_take_setting_conflict(void);

/* The refusal-token vocabulary, so a caller can build a histogram with a
 * denominator instead of one that only has rows for what it happened to see.
 * A kind with no row reads as "never happened" rather than as "not counted",
 * and the flip criterion is read per kind, so that difference is the
 * measurement.
 *
 * This is also the single list. The command layer on this side of the ABI
 * raises refusals of its own, before the evaluator is reached, and their
 * tokens are in here rather than in a second list maintained by hand --
 * two vocabularies drift the moment either side gains a kind. Use
 * ixe_refusal_token_raised_by to tell them apart.
 *
 * Names are static storage: do not free them. */
size_t ixe_refusal_token_count(void);
const char * ixe_refusal_token_at(size_t index);

/* 0 the evaluator raises it, 1 the command layer does, 2 either, 3 nobody --
 * a sentinel such as `unrecorded`, which names a MISSING token rather than a
 * kind of refusal. Negative if index is out of range.
 *
 * 3 is distinct from 2 on purpose: a consumer asking "is this one of the
 * command layer's?" must not be told yes for a name nobody raises. */
int ixe_refusal_token_raised_by(size_t index);

/* What builtins.nixVersion reports. Passed in rather than compiled into the
 * Rust crate so there is only one copy of the version number. */
int ixe_set_nix_version(const unsigned char * v, size_t v_len);

/* Immutable host artifact identity, including the host implementation and its
 * dependencies. Required for persistent caching with external host callbacks.
 * Must be nonempty UTF-8. Identical repeats are accepted; a conflicting value
 * fails and reports its diagnostic through ixe_take_setting_conflict. */
int ixe_set_host_build_identity(const unsigned char * identity, size_t identity_len);

/* What builtins.currentSystem reports, from settings.thisSystem. Passed in
 * because --system and nix.conf both move it, so a value derived from this
 * crate's build target would disagree with the arm it is compared against.
 * It is the first thing a real package set reads: `import <nixpkgs> {}` needs
 * it before anything else. */
int ixe_set_current_system(const unsigned char * v, size_t v_len);

/* The store directory derivations are built under, from store->storeDir. It
 * is hashed into every store path derivationStrict computes, not merely
 * prefixed onto one, so without this the primop refuses instead of assuming
 * /nix/store and producing a path that is wrong in every character while
 * looking well-formed. */
int ixe_set_store_dir(const unsigned char * dir, size_t dir_len);

/* What a `~/...` path literal expands to, from getHome(). Passed in because
 * getHome() is not getenv("HOME"): it stats the directory, falls back to the
 * passwd entry when $HOME is unset or names a directory this euid does not
 * own, and warns when it does. A second implementation of that rule in the
 * Rust crate would resolve to a different file rather than to none, so the
 * embedder answers it. Without this the crate falls back to $HOME, which is
 * what the standalone probe and the examples get. */
int ixe_set_home_dir(const unsigned char * dir, size_t dir_len);

/* ---- the handle API --------------------------------------------------- */

/* An evaluation session: one VM, the values it produced, and the message
 * belonging to its most recent failure. Opaque; only ever held by pointer.
 * Declared above, before the first function that takes one. */

/* A value inside one session, named by an opaque integer.
 *
 * Ownership, all of it:
 *   - every call that writes a handle transfers ownership to the caller;
 *   - release one with ixe_handle_free, or all of them with ixe_session_free;
 *   - a handle belongs to the session that made it. Its high bits carry that
 *     session's serial, so passing it to another session is reported as a bad
 *     call rather than silently naming that session's first value;
 *   - zero is never a valid handle;
 *   - freeing an unknown or already-freed handle is a no-op.
 * Freeing a handle does not free the value: another handle, or the value
 * graph itself, may still reference it. The session owns the graph. */
typedef uint64_t IxeHandle;

/* Statuses beyond the six above: */
#define IXE_ERR_MISSING 7 /* no such attribute, or index past the end */
#define IXE_ERR_IFD 8     /* import from derivation disabled (IFDError) */
/* a top-level auto-call met a formal with neither a default nor an
 * argument (MissingArgumentError) */
#define IXE_ERR_MISSING_ARGUMENT 9
/* all selection candidates are missing (AttrPathNotFound); unlike the
 * optional field/index sentinel, carries a complete diagnostic */
#define IXE_ERR_ATTR_PATH_NOT_FOUND 10

/* ixe_value_type results. Negative values are not Nix types; they say the
 * question could not be answered. */
#define IXE_TYPE_UNKNOWN_HANDLE (-1)
#define IXE_TYPE_UNFORCED (-2) /* a thunk: call ixe_force first */
#define IXE_TYPE_INT 0
#define IXE_TYPE_FLOAT 1
#define IXE_TYPE_BOOL 2
#define IXE_TYPE_NULL 3
#define IXE_TYPE_STRING 4
#define IXE_TYPE_PATH 5
#define IXE_TYPE_LIST 6
#define IXE_TYPE_ATTRS 7
#define IXE_TYPE_FUNCTION 8

/* ixe_render modes. All three render on the Rust side, because all three
 * already exist there and are compared against cppnix by the lang corpus:
 * PLAIN is the printer every eval-okay file is diffed through, JSON is
 * builtins.toJSON's walker (the same __toString and outPath rules
 * printValueAsJSON applies), RAW is coerceToString with coerceMore = false,
 * which is what `nix eval --raw` passes. */
#define IXE_RENDER_PLAIN 0
#define IXE_RENDER_JSON 1 /* compact; re-dump it if you want --pretty */
#define IXE_RENDER_RAW 2
/* `nix eval`'s plain output. cppnix has two plain printers and they are not
 * the same function: nix-instantiate uses printAmbiguous, `nix eval` uses
 * ValuePrinter. Measured over 46 expressions they agree on 44; this mode
 * refuses the two they do not agree on rather than printing the other
 * dialect's answer. */
#define IXE_RENDER_VALUE_PRINTER 3
/* `nix-instantiate --eval --strict --xml --no-location`: builtins.toXML's
 * walker, which is the same printValueAsXML cppnix calls for both once
 * --no-location turns the source positions off. The document already ends
 * in a newline; print it without appending one. */
#define IXE_RENDER_XML 4
/* nix-instantiate --eval without --strict: the value in weak head normal
 * form, served only when it has no children (a string, a number, a Boolean,
 * null, a path), for which lazy and strict printing are one answer; a value
 * with children is refused with the `lazy-print` token, because which
 * children cppnix prints as <CODE> is evaluator-internal. Its own mode so
 * that a row rendered strictly is never served to a lazy request. */
#define IXE_RENDER_PLAIN_LAZY 5

/* Create a session that answers through `host`.
 *
 * The struct is copied, so the caller may free it as soon as this returns --
 * but everything it points at, `ctx` and any buffer a hook writes into
 * included, must outlive the session.
 *
 * Returns NULL when `host` is NULL, and when it is malformed, which today
 * means a partial set of the seven path reads; ixe_take_setting_conflict then
 * carries the reason. A session with no host is not a useful object, and
 * accepting one would put the "which embedder answers this" question back
 * where it was. */
IxeSession * ixe_session_new(const IxeHostVtable * host);

/* Explicit bounded decoded-witness owner. Budgets are accounted payload bytes
 * and entries; zero disables retention. Cache and sessions stay on one thread.
 * No VM values or host contexts are retained. */
IxeEvalCache * ixe_eval_cache_new(uint64_t max_bytes, size_t max_entries);
/* Attached sessions hold their own reference; NULL is accepted. */
void ixe_eval_cache_free(IxeEvalCache * cache);
/* Non-null pointers must be valid; NULL cache or output returns BADCALL. */
int ixe_eval_cache_stats(const IxeEvalCache * cache, IxeEvalCacheStats * output);
/* Attach to an idle session, or detach with NULL. Retains shared ownership. */
int ixe_session_set_eval_cache(IxeSession * session, const IxeEvalCache * cache);

/* Root for file-backed questions: empty for ambient, otherwise a canonical
 * mounted store path. Refused while a question is in flight. */
int ixe_session_set_source_root(IxeSession * session, const unsigned char * root, size_t root_len);
void ixe_session_free(IxeSession * session);

/* The message belonging to this session's most recent non-zero status, or
 * NULL. Ownership transfers; free with ixe_string_free. Taking it clears it,
 * so one message is never reported twice against two different calls.
 *
 * out_pos receives where the failure happened, or the "nowhere" value when it
 * has none; NULL when the caller does not want it. Taken here rather than
 * through an accessor of its own because the message and its position are one
 * fact: two calls could be made in either order, and the order that asks for
 * the position after taking the message gets nothing, silently, in a shape
 * whose output still looks like a complete error. */
char * ixe_session_take_error(IxeSession * session, IxePos * out_pos);

/* The next complaint about a damaged cache entry, or NULL when there are no
 * more. A damaged entry is a slower evaluation, not a wrong answer, so it
 * never replaces the result; the embedder decides where these go. Ownership
 * transfers. */
char * ixe_session_take_warning(IxeSession * session);

/* Evaluate source and write a handle to the result in weak head normal form,
 * WITHOUT memoising anything.
 * `{ a = 1; b = throw "x"; }` succeeds here: the throw waits until something
 * asks for b. On a non-zero return *out is untouched.
 *
 * The compile cache applies; the result cache cannot. A memo row is filed
 * under the question that was asked and this call has not been told one, so
 * everything reached through the returned handle is evaluated from cold on
 * every run however warm eval-cache-dir is. A caller that knows its question
 * -- which is every command in this tree -- wants
 * ixe_session_eval_question below. */
int ixe_session_eval(
    IxeSession * session,
    const unsigned char * src,
    size_t src_len,
    const unsigned char * base_dir, /* directory for relative paths; NULL = cwd */
    size_t base_dir_len,
    /* absolute path the source was read from, or NULL when it was not read
     * from one (--expr). This is what __curPos reports; cppnix answers null
     * for a string origin rather than naming a file, so NULL and a path are
     * different answers and not a default. ENG-12713. */
    const unsigned char * file,
    size_t file_len,
    IxeHandle * out);

/* One whole question: which source, which attribute path, which output shape.
 *
 * Stating all of it up front is what lets eval-cache-dir serve `nix eval` and
 * `nix build` at all. While the memo key was (module, settings) the only
 * caller it could serve was the one whose question never varies -- render the
 * whole expression -- so the two commands people run wrote module objects
 * into the cache directory and read nothing back for the life of the setting
 * (ENG-12830).
 *
 * Writes one of the IXE_SERVE_* values through out_mode:
 *
 *   IXE_SERVE_ANSWER   *out_answer is the memoised answer, *out_root is 0.
 *                      The caller is done and must not walk anything.
 *   IXE_SERVE_EVALUATE *out_root is the expression -- in weak head normal
 *                      form when there are no arguments, and an unforced
 *                      application of it to them when there are -- and
 *                      *out_answer is NULL. Walk it, then report what you
 *                      produced to ixe_session_question_answer.
 *   IXE_SERVE_VERIFY   both are set. This is one of the sampled checks of a
 *                      memoised answer: do the work anyway, report it for
 *                      comparison, then use *out_answer -- the served one --
 *                      so a command's output never depends on whether the
 *                      sampler picked it.
 *
 * Between the two calls every force in this session is recorded, so the read
 * set covers the walk and the render and not only the first evaluation. That
 * is also what carries the .drv write on a later hit: it leaves the evaluator
 * as a store question and replaying the read set re-performs it (ENG-12801).
 *
 * Only successful answers are memoised; see ixe_session_question_answer.
 *
 * *out_answer is owned by the caller and freed with ixe_string_free. */
#define IXE_QUESTION_SELECT 0          /* walk attr_paths, then render */
#define IXE_QUESTION_DERIVATION 1      /* walk attr_paths, then read a derivation */
#define IXE_QUESTION_APP 2             /* walk attr_paths, then read an application */
#define IXE_QUESTION_FLAKE_SHOW 3      /* lazily describe a flake output tree */
#define IXE_QUESTION_DERIVATION_PATH 4 /* read only a derivation path */
/* walk EVERY attr path (a list to visit, not a ladder) and report every
 * derivation cppnix's getDerivations reaches from each: nix-build */
#define IXE_QUESTION_DERIVATION_SET 5
/* read the source as a flake.nix: the root must be a set literal (the
 * question fails before evaluating otherwise, as evalFile(mustBeTrivial)
 * does), and the answer is ixe_flake_document's JSON. Pass one empty attr
 * path; there is nothing to select. */
#define IXE_QUESTION_FLAKE_DOCUMENT 6
/* select a package, then strictly read its meta.position */
#define IXE_QUESTION_SOURCE_POSITION 7
/* unfiltered package metadata; render=0 selects a candidate, render=1 unions existing roots */
#define IXE_QUESTION_SEARCH_PACKAGES 8
/* strict flake output validation; render carries the checked scope options */
#define IXE_QUESTION_FLAKE_CHECK 9

#define IXE_SERVE_EVALUATE 0
#define IXE_SERVE_ANSWER 1
#define IXE_SERVE_VERIFY 2

/* A counted byte string the caller owns for the length of the call. */
typedef struct
{
    const unsigned char * text;
    size_t len;
} IxeBytes;

/* Argument kinds for IxeArgument below. */
#define IXE_ARG_JSON 0            /* a document in ixe_alloc_json's dialect */
#define IXE_ARG_INTERNAL_PRIMOP 1 /* the name of one of cppnix's internal primops */

/* One value to apply to the source before the question is asked of the
 * result. `nix eval <flake>#attr` evaluates cppnix's call-flake.nix applied
 * to a lock file, an overrides document and an internal primop; these are
 * those, and nothing here knows a flake is being built.
 *
 * They cross on the question call rather than through ixe_alloc_json and
 * ixe_apply afterwards, and that is a soundness requirement rather than a
 * convenience. The memo key is built from this list, and the same list is
 * what gets applied -- so there is no arrangement of embedder code that keys
 * on one thing and applies another. The three value-building calls therefore
 * refuse while a question is in flight (IXE_ERR_BADCALL): a value injected
 * that way would be in no key while its reads went into the read set of a row
 * that is, and the row would later be served for a different value.
 * ENG-12915. */
typedef struct
{
    int kind; /* IXE_ARG_* */
    IxeBytes text;
} IxeArgument;

#define IXE_AUTO_ARG_EXPR 0   /* --arg: an expression, parsed under the working directory */
#define IXE_AUTO_ARG_STRING 1 /* --argstr: the bytes, as a string without context */

/* One --arg/--argstr: cppnix's autoArgs entry. What a function met on the
 * walk is applied to (findAlongAttrPath auto-calls before every component,
 * getDerivations at every level, nix-instantiate --eval once more at the
 * end), so the list is in the memo key whole. */
typedef struct
{
    IxeBytes name;
    int kind; /* IXE_AUTO_ARG_* */
    IxeBytes text;
} IxeAutoArg;

int ixe_session_eval_question(
    IxeSession * session,
    const unsigned char * src,
    size_t src_len,
    const unsigned char * base_dir, /* directory for relative paths; NULL = cwd */
    size_t base_dir_len,
    /* as for ixe_session_eval: the path the source was read from, or NULL */
    const unsigned char * file,
    size_t file_len,
    /* applied to the source in order before anything is selected; NULL/0 to
     * apply nothing. *out_root then names the application, which is lazy: a
     * flake's outputs have not run when this returns. */
    const IxeArgument * args,
    size_t args_len,
    int kind, /* IXE_QUESTION_* */
    /* dotted attribute paths tried in order; the first that resolves is the
     * one selected, which is what makes `nixpkgs#hello` mean
     * `legacyPackages.<sys>.hello`. Pass one for --expr and --file, and one
     * empty path for the root itself. An empty array is a bad call rather
     * than a spelling of the root: it would be a second spelling of one walk,
     * and the key would then describe a ladder that resolves nothing.
     *
     * The whole ladder is in the memo key, not its first entry: which
     * candidate resolves depends on the value, so two commands whose ladders
     * share a head and differ after it can reach different attributes. */
    const IxeBytes * attr_paths,
    size_t attr_paths_len,
    /* whether an all-digit path component indexes a list (cppnix's
     * findAlongAttrPath) rather than naming an attribute (a flake's
     * AttrCursor::findAlongAttrPath). Also in the key. */
    int index_lists,
    /* `nix eval --apply`: an expression applied to the selected value before
     * the answer, or NULL/0 for none. In the key, together with the working
     * directory it is parsed under (cppnix's rootPath(".")). The embedder
     * applies it with ixe_question_apply below, which is the one application
     * allowed while the question is in flight, because it is the one the key
     * names. */
    const unsigned char * apply,
    size_t apply_len,
    /* --arg/--argstr, or NULL/0 for none. In the key. A --arg is parsed now
     * (a parse failure fails the call before anything can be served) and
     * evaluated when a formal first reads it. The embedder applies them
     * with ixe_auto_call below, wherever cppnix auto-calls. */
    const IxeAutoArg * auto_args,
    size_t auto_args_len,
    /* whether the selected value is auto-called once more at the end, as
     * nix-instantiate --eval does when it has arguments and nix eval never
     * does. In the key. The embedder makes the call; this only names it. */
    int auto_call,
    int render, /* IXE_RENDER_*, or IXE_QUESTION_FLAKE_SHOW option bits */
    int * out_mode,
    IxeHandle * out_root,
    char ** out_answer);

/* File the answer the caller produced for the question in flight, or compare
 * it against the one that was served.
 *
 * `status` is 0 when the caller produced an answer and the status it is about
 * to raise otherwise. A non-zero status abandons the question without filing
 * anything: a failure on this path can be raised by the bridge rather than by
 * the evaluator -- a missing attribute carries the sibling names it suggests,
 * a refusal carries a token -- and none of those round-trip through the
 * (status, text) pair a row holds, so storing one would be storing something
 * this cannot reproduce. ENG-12857.
 *
 * Calling this with no question in flight does nothing and is not an error,
 * so a caller that was served and one that was not can call it on the same
 * line, and neither branch can forget it. */
int ixe_session_question_answer(IxeSession * session, int status, const unsigned char * answer, size_t answer_len);

/* The embedder could not use the answer the last ixe_session_eval_question
 * served (it did not decode as the shape the question promised). Forgets that
 * memo row and its witness, on disk too, so asking the question again
 * evaluates and no later process is served it either. `why` is reported
 * through the session's warnings. Fails when nothing was served or the store
 * refused the removal. */
int ixe_session_question_reject(IxeSession * session, const unsigned char * why, size_t why_len);

/* Force a handle to weak head normal form. Idempotent: the cell memoises, so
 * a repeat is free and a failed force raises the same error rather than
 * running it again. */
int ixe_force(IxeSession * session, IxeHandle handle);

/* What an already-forced handle is. Reports IXE_TYPE_UNFORCED rather than
 * forcing, so asking what something is cannot enter a thunk by accident. */
int ixe_value_type(IxeSession * session, IxeHandle handle);

void ixe_handle_free(IxeSession * session, IxeHandle handle);

/* Attribute sets. Counting and naming do not force: a set's names are known
 * once the set is, and nothing about a sibling's value is read. Names are
 * ordered the way cppnix orders them, by name rather than by symbol id.
 *
 * ixe_attrs_names hands back every name in ONE crossing: `*out` is a buffer of
 * `*out_len` bytes holding the names back to back, each NUL-terminated
 * including the last, and an empty set gives a null pointer with length zero.
 * Free it with ixe_names_free, not ixe_string_free -- the buffer has a NUL
 * after every name, so strlen would measure only the first one.
 *
 * There is deliberately no per-index accessor. The names live in a map ordered
 * by symbol id, so answering "the name at index i" in cppnix's order means
 * materialising and sorting the whole list with nowhere to keep it between
 * calls, which made enumerating nixpkgs' 25,442-name top level quadratic and
 * an attribute-not-found error take 42 seconds against cppnix's 2 for an
 * identical message. ENG-12913. */
int ixe_attrs_len(IxeSession * session, IxeHandle handle, size_t * out);
int ixe_attrs_names(IxeSession * session, IxeHandle handle, char ** out, size_t * out_len);
void ixe_names_free(char * names, size_t len);

/* Select one attribute without forcing it or any sibling. IXE_ERR_MISSING
 * when there is no such attribute, with the name available from
 * ixe_session_take_error so the caller can build cppnix's message. */
int ixe_attrs_select(
    IxeSession * session, IxeHandle handle, const unsigned char * name, size_t name_len, IxeHandle * out);

/* Lists. An attribute path may index one, which is why ixe_list_at exists
 * beside ixe_attrs_select. IXE_ERR_MISSING past the end. */
int ixe_list_len(IxeSession * session, IxeHandle handle, size_t * out);
int ixe_list_at(IxeSession * session, IxeHandle handle, size_t index, IxeHandle * out);

/* Scalars. Each forces the handle first, then refuses with IXE_ERR_BADCALL if
 * the value is not of that type, naming what it found instead. */
int ixe_get_int(IxeSession * session, IxeHandle handle, int64_t * out);
/* Not widened from an integer: Nix keeps `1` and `1.0` apart, and promoting
 * here would hide from a caller which one it had. */
int ixe_get_float(IxeSession * session, IxeHandle handle, double * out);
int ixe_get_bool(IxeSession * session, IxeHandle handle, int * out);
/* Strings and paths. A string carrying context is refused rather than handed
 * over without it: the bytes of `"${./f}"` are a store path and the context
 * is the record that the value depends on it, so bare bytes are a value that
 * looks complete and has lost the dependency. Letting context cross is
 * ENG-12492. */
int ixe_get_string(IxeSession * session, IxeHandle handle, char ** out);
/* Return a string's context as NUL-terminated NixStringContextElem spellings.
 * An empty context yields NULL/0. The buffer is owned by the caller and is
 * released with ixe_names_free. */
int ixe_get_string_context(IxeSession * session, IxeHandle handle, char ** out, size_t * out_len);

/* Render a value to the bytes a command prints. Forces deeply, which is what
 * each of these output modes does in cppnix too, so a throw in the selected
 * subtree happens here. Ownership of *out transfers. */
int ixe_render(IxeSession * session, IxeHandle handle, int mode, char ** out);

/* Values the embedder builds and hands in, for a command whose program is a
 * function of data cppnix computed: `nix eval <flake>#attr` evaluates
 * cppnix's own call-flake.nix applied to a lock file, an overrides set and an
 * internal primop, none of which the VM can produce because locking a flake
 * is IO and policy the embedder owns.
 *
 * Three general calls, not one ixe_call_flake. Nothing here knows what a
 * flake is: one applies, one decodes JSON, one names an internal primop.
 * Each mirrors something cppnix already has (callFunction, parseJSON,
 * internalPrimOps), and the bridge builds a flake out of them.
 *
 * All three refuse with IXE_ERR_BADCALL while a question is in flight. A
 * value built between ixe_session_eval_question and
 * ixe_session_question_answer is in no memo key, and every force after it is
 * recorded into the read set of a row that is -- so the row would later be
 * served for a different value. That is why the arguments a flake needs are
 * passed to the question call itself, which keys on them. ENG-12915. */

/* A value from a JSON document, read as builtins.fromJSON reads it, with one
 * addition JSON cannot express: an object of the form
 * {"__storePath": "/nix/store/..."} becomes a string carrying that path as
 * its own context. Without the escape a store path handed over as a plain
 * string prints correctly and has lost the dependency it stands for, so a
 * derivation built from it ends up with one fewer input. An object carrying
 * the key beside any other key, or with a non-string value under it, is an
 * error rather than a plain attribute set. */
int ixe_alloc_json(IxeSession * session, const unsigned char * json, size_t json_len, IxeHandle * out);

/* One of cppnix's internal primops by its registered name -- its
 * state.internalPrimOps, with the same one member today, fetchFinalTree. A
 * primop registered ordinarily is refused: it is already reachable as
 * builtins.<name>, and a second spelling here would differ in whether the
 * gate applies. */
int ixe_internal_primop(IxeSession * session, const unsigned char * name, size_t name_len, IxeHandle * out);

/* Apply the question's `apply` expression (see ixe_session_eval_question) to
 * a value, lazily, the way ixe_apply does. Allowed while the question is in
 * flight -- the expression and its directory are in the question's key, so
 * the row filed for the question is filed for this application too -- and a
 * bad call with no such expression in flight. */
int ixe_question_apply(IxeSession * session, IxeHandle arg, IxeHandle * out);

/* cppnix's autoCallFunction on `value`, under the --arg/--argstr of the
 * question in flight: a set with __functor is applied to itself and the
 * result treated the same way; a lambda with formals is applied to the set
 * its formals select from the arguments (everything under an ellipsis,
 * otherwise each formal's own or its default; neither is
 * IXE_ERR_MISSING_ARGUMENT); anything else comes back as it was. Lazy like
 * ixe_apply: force the result where cppnix forces. Refused outside a
 * question (IXE_ERR_BADCALL): the arguments are in that question's key. */
int ixe_auto_call(IxeSession * session, IxeHandle value, IxeHandle * out);

/* cppnix's getDerivations from `root` (get-drvs.cc: auto-called at every
 * level, attributes in name order and only path components, a set entered
 * under recurseForDerivations = true or a parent's _combineChannels, every
 * list element, each derivation once by identity, name forced), under the
 * question in flight. *out is the records found, two netstrings per record
 * -- `<len>:<drvPath>,<len>:<outputName>,` -- so no byte in a name can
 * spell a record boundary; records concatenate across calls. Free with
 * ixe_string_free. */
int ixe_derivation_set(IxeSession * session, IxeHandle root, char ** out);

/* Evaluate and normalize a FlakeDocument question. The JSON has
 * description (null or string), inputs, self_attrs, and config. Each input
 * has reference (null or tagged url/attrs/implicit), is_flake, follows
 * (null or parsed path components), and overrides. Fetch/configuration
 * leaves are tagged values; path values retain their mounted root.
 *
 * Rust owns validation, reference conflicts, follows syntax, self rules,
 * configuration types and implicit inputs from output formals. Computed
 * metadata is forced through the same read-tracked memo as evaluation;
 * the outputs function is validated but never invoked. The host only
 * translates normalized declarations into fetch/store operations.
 * Free the returned bytes with ixe_string_free. */
int ixe_flake_document(IxeSession * session, IxeHandle root, char ** out);

/* Apply a function to one argument. cppnix's callFunction, minus the forcing:
 * the function is forced (cppnix answers "is this a function" at the call),
 * the argument is not, and the result is a lazy cell. That is what lets
 * `nix eval <flake>#lib.version` apply call-flake.nix to three arguments and
 * then enter exactly `lib` and `version`. Curry by calling again. */
int ixe_apply(IxeSession * session, IxeHandle func, IxeHandle arg, IxeHandle * out);

#ifdef __cplusplus
}
#endif
