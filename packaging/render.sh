#!/bin/sh
# Fills a package template (packaging/*/*.in) for one release and prints it:
#
#   sh packaging/render.sh <template> <tag> <checksums-sha256.txt>
#
# @TAG@ becomes the tag (v2.0.0), @VERSION@ the tag without its v, and @SHA256_<target>@
# the checksum of that target's archive (.tar.gz or .zip) in the sums file. A target the sums
# file lacks is an error, so a release missing an archive never produces a package.

set -eu

[ $# -eq 3 ] || {
    echo "usage: render.sh <template> <tag> <checksums-sha256.txt>" >&2
    exit 2
}

awk -v tag="$2" -v version="${2#v}" '
    FILENAME == ARGV[1] {
        hash[$2] = $1
        next
    }
    {
        line = $0
        out = ""
        while (match(line, /@SHA256_[A-Za-z0-9_.-]+@/)) {
            target = substr(line, RSTART + 8, RLENGTH - 9)
            sum = hash["glidesh-" tag "-" target ".tar.gz"]
            if (sum == "") sum = hash["glidesh-" tag "-" target ".zip"]
            if (sum == "") {
                print "render.sh: no archive for " target " in the sums file" > "/dev/stderr"
                failed = 1
                exit
            }
            out = out substr(line, 1, RSTART - 1) sum
            line = substr(line, RSTART + RLENGTH)
        }
        line = out line
        gsub(/@TAG@/, tag, line)
        gsub(/@VERSION@/, version, line)
        print line
    }
    END {
        if (failed) exit 1
    }
' "$3" "$1"
