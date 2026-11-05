#!/usr/bin/env bash
# Prints the GitHub release notes for VERSION from CHANGELOG.md: that
# version's section merged with anything still under [Unreleased]. A
# rebuild of an already-released version (changes pushed without a version
# bump) replaces its binaries, so its notes must cover those changes too.
# Only the newest version (or one with no section yet) takes them.
# Headings come out in Keep-a-Changelog order; newer ([Unreleased]) bullets
# first under each. Prints nothing if neither section has bullets.
#
# Usage: scripts/release-notes.sh VERSION [CHANGELOG]
set -euo pipefail

version="${1:?usage: release-notes.sh VERSION [CHANGELOG]}"
changelog="${2:-CHANGELOG.md}"

awk -v v="$version" '
function emit(heading,    body) {
    done[heading] = 1
    body = (take_newer ? newer[heading] : "") older[heading]
    if (body == "") return
    if (heading != "") printf "### %s\n", heading
    printf "%s\n", body
}
/^## \[/ {
    section = ""
    if (index($0, "## [Unreleased]") == 1) section = "newer"
    else {
        if (newest == "") newest = $0
        if (index($0, "## [" v "]") == 1) { section = "older"; found = $0 }
    }
    heading = ""
    next
}
section == "" { next }
/^### / { heading = substr($0, 5); seen[heading] = 1; next }
/^[[:space:]]*$/ { next }
{
    seen[heading] = 1
    if (section == "newer") newer[heading] = newer[heading] $0 "\n"
    else older[heading] = older[heading] $0 "\n"
}
END {
    take_newer = (found == "" || found == newest)
    # Lines right under the version heading (no subsection) lead.
    emit("")
    n = split("Added Changed Deprecated Removed Fixed Security", order, " ")
    for (i = 1; i <= n; i++) emit(order[i])
    for (heading in seen) if (!(heading in done)) emit(heading)
}
' "$changelog"
