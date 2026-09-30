# Literal ordinary [workspace] members/exclude arrays, including multiline arrays.
/^[[:space:]]*#/ { next }
/^[[:space:]]*\[/ { section=$0; sub(/[[:space:]]*#.*/, "", section); active=0 }
section ~ /^[[:space:]]*\[workspace\][[:space:]]*$/ {
    line=$0
    sub(/[[:space:]]*#.*/, "", line)
    if (line ~ "^[[:space:]]*" key "[[:space:]]*=") {
        active=1
        sub(/^[^=]*=[[:space:]]*\[/, "", line)
    }
    if (active) {
        while (match(line, /["\047][^"\047]+["\047]/)) {
            print substr(line, RSTART+1, RLENGTH-2)
            line=substr(line, RSTART+RLENGTH)
        }
        if (line ~ /\]/) active=0
    }
}
