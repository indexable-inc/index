#!/usr/bin/env bash

source common.sh

python3 "$functionalTestsDir/legacy-ssh-gc.py" "$(command -v nix)"
