#!/usr/bin/env bash

source common.sh

clearStoreIfPossible

# The flake needs an identity to be fetchable at all, and the tree of a jj
# workspace is everything on disk under it -- which rules out $TEST_HOME,
# where Nix writes its caches and state while the test runs.
searchRoot=$TEST_ROOT/search-root
jjFlakeDir "$searchRoot"
writeSimpleFlake "$searchRoot"
cd "$searchRoot"
mkdir -p foo/subdir

# Every `nix build` below writes a `result` symlink into the tree it is
# building from. Ignoring it keeps the flake's tree id fixed across the run,
# so the repeated builds fetch one source instead of a fresh one each time.
echo result > .gitignore

echo '{ outputs = _: {}; }' > foo/flake.nix
cat <<EOF > flake.nix
{
    inputs.foo.url = "jj+file://$PWD?dir=foo";
    outputs = a: {
       packages.$system = rec {
         test = import ./simple.nix;
         default = test;
       };
    };
}
EOF
mkdir subdir
pushd subdir

success=("" . .# .#test ../subdir ../subdir#test "$PWD")
failure=("path:$PWD" "../simple.nix")

for i in "${success[@]}"; do
    nix build "$i" || fail "flake should be found by searching up directories"
done

for i in "${failure[@]}"; do
    ! nix build "$i" || fail "flake should not search up directories when using 'path:'"
done

# A repository boundary stops the upward search for a flake.nix, and a jj
# workspace is a repository just as a Git checkout is. Checked here, while
# every entry above still builds, so that the boundary is the only thing that
# changes: the same assertion made further down, after the input has been
# broken, would pass whether or not the boundary existed.
#
# `.jj` is created as a bare directory rather than by `jjInit`, because
# flakeref.cc decides on its presence and nothing else, and initialising a
# real workspace inside the surrounding one would be a different thing to
# reason about.
mkdir .jj
for i in "${success[@]}" "${failure[@]}"; do
    ! nix build "$i" || fail "flake should not search past a jj workspace"
done
rmdir .jj

popd

nix build --override-input foo . || fail "flake should search up directories when not an installable"

sed "s,dir=foo,dir=foo/subdir,g" -i flake.nix
! nix build || fail "flake should not search upwards when part of inputs"

if [[ -n $(type -p git) ]]; then
    pushd subdir
    git init
    for i in "${success[@]}" "${failure[@]}"; do
        ! nix build "$i" || fail "flake should not search past a git repository"
    done
    rm -rf .git
    popd
fi
