#pragma once
///@file

#include "nix/util/types.hh"
#include "nix/util/serialise.hh"
#include "nix/util/fs-sink.hh"

namespace nix {

/**
 * dumpPath creates a Nix archive of the specified path.
 *
 * @param path the file system data to dump. Dumping is recursive so if
 * this is a directory we dump it and all its children.
 *
 * @param [out] sink The serialised archive is fed into this sink.
 *
 * @param filter Can be used to skip certain files.
 *
 * The format is as follows:
 *
 * ```
 * IF path points to a REGULAR FILE:
 *   dump(path) = attrs(
 *     [ ("type", "regular")
 *     , ("contents", contents(path))
 *     ])
 *
 * IF path points to a DIRECTORY:
 *   dump(path) = attrs(
 *     [ ("type", "directory")
 *     , ("entries", concat(map(f, sort(entries(path)))))
 *     ])
 *     where f(fn) = attrs(
 *       [ ("name", fn)
 *       , ("file", dump(path + "/" + fn))
 *       ])
 *
 * where:
 *
 *   attrs(as) = concat(map(attr, as)) + encN(0)
 *   attrs((a, b)) = encS(a) + encS(b)
 *
 *   encS(s) = encN(len(s)) + s + (padding until next 64-bit boundary)
 *
 *   encN(n) = 64-bit little-endian encoding of n.
 *
 *   contents(path) = the contents of a regular file.
 *
 *   sort(strings) = lexicographic sort by 8-bit value (strcmp).
 *
 *   entries(path) = the entries of a directory, without `.` and
 *   `..`.
 *
 *   `+` denotes string concatenation.
 * ```
 */
void dumpPath(const std::filesystem::path & path, Sink & sink, PathFilter & filter = defaultPathFilter);

/**
 * Dump an archive with a single file with these contents.
 *
 * @param s Contents of the file.
 */
void dumpString(std::string_view s, Sink & sink);

void parseDump(FileSystemObjectSink & sink, Source & source);

void restorePath(const std::filesystem::path & path, Source & source, bool startFsync = false);

/**
 * Read a NAR from 'source' and write it to 'sink'.
 */
void copyNAR(Source & source, Sink & sink);

inline constexpr std::string_view narVersionMagic1 = "nix-archive-1";

inline constexpr std::string_view caseHackSuffix = "~nix~case~hack~";

/**
 * If the `use-case-hack` setting is enabled and `name` carries the
 * case-hack suffix applied by `restorePath()`, return the name with
 * the suffix removed; `std::nullopt` otherwise. Every serializer that
 * re-reads a restored tree must apply this (the NAR dump does it
 * inline), or a tree that took the hack round-trips to a different
 * hash than the one it was ingested under.
 */
std::optional<std::string> stripCaseHackSuffix(std::string_view name);

/**
 * Set the *default* of the `use-case-hack` setting; a no-op if the
 * user set it explicitly. Called during store initialization once the
 * store location is known: the hack is only needed when the store
 * lives on a case-insensitive filesystem, so a case-sensitive store
 * (e.g. a "Case-sensitive APFS" volume) defaults it off and keeps the
 * true file names on disk.
 */
void setDefaultUseCaseHack(bool enable);

} // namespace nix
