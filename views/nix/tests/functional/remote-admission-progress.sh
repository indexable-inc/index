#!/usr/bin/env bash

source common.sh

[[ "$system" == *-linux ]] || skipTest "Linux pipe-capacity regression"

python3 "$functionalTestsDir/remote-admission-progress.py" "$(command -v nix)"
