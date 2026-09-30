# Static local graph. Ordinary dependency tables/inline tables, including aliases.
function trim(s) { sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s); return s }
function quoted(s, key,    expr) {
    expr=key "[[:space:]]*=[[:space:]]*[\"\047][^\"\047]+[\"\047]"
    if (match(s, expr)) {
        s=substr(s, RSTART, RLENGTH); sub(/^[^=]*=[[:space:]]*["\047]/, "", s)
        sub(/["\047]$/, "", s); return s
    }
    return ""
}
function dependency(alias, text,    pkg) {
    if (!alias) return
    pkg=quoted(text, "package"); if (!pkg) pkg=alias
    if (section=="workspace.dependencies" || depsection=="workspace.dependencies") {
        if (text ~ /path[[:space:]]*=/) inherited[alias]=pkg
    } else if (text ~ /path[[:space:]]*=/ || text ~ /workspace[[:space:]]*=[[:space:]]*true/) {
        dep[source,alias]=pkg
        if (text ~ /workspace[[:space:]]*=[[:space:]]*true/) inherit[source,alias]=1
    }
}
FNR==1 {
    dependency(alias,deptext); source=FILENAME
    section=""; alias=""; deptext=""; depsection=""; files[++n]=FILENAME
}
/^[[:space:]]*#/ { next }
/^[[:space:]]*\[/ {
    dependency(alias, deptext); alias=""; deptext=""; depsection=""
    section=trim($0); sub(/[[:space:]]*#.*/, "", section)
    sub(/^\[/, "", section); sub(/\][[:space:]]*$/, "", section)
    if (section ~ /(^|\.)(dependencies|build-dependencies|dev-dependencies)\.[^.]+$/) {
        alias=section; sub(/^.*\./,"",alias)
        depsection=section; sub(/\.[^.]+$/, "", depsection)
    }
    next
}
section=="package" && /^[[:space:]]*name[[:space:]]*=/ { name[FILENAME]=quoted($0,"name") }
{
    if (alias) deptext=deptext " " $0
    else if (section ~ /(^|\.)(dependencies|build-dependencies|dev-dependencies)$/ && $0 ~ /=/) {
        key=$0; sub(/=.*/,"",key); key=trim(key); gsub(/["\047]/,"",key)
        dependency(key,$0)
    }
}
function longest(pkg,    other, span, best, bestpath) {
    if (visiting[pkg]) { cycle=1; return 0 }
    if (depth[pkg]) return depth[pkg]
    visiting[pkg]=1; best=1; bestpath=pkg
    for (other in packages) if (edge[pkg,other]) {
        span=longest(other)+1
        if (span>best || (span==best && pkg " -> " path[other]<bestpath)) {
            best=span; bestpath=pkg " -> " path[other]
        }
    }
    visiting[pkg]=0; depth[pkg]=best; path[pkg]=bestpath; return best
}
END {
    dependency(alias,deptext)
    for (i=1;i<=n;i++) if (name[files[i]]) packages[name[files[i]]]=1
    for (pair in dep) {
        split(pair,parts,SUBSEP); pkg=dep[pair]
        if (inherit[pair]) { if (!(parts[2] in inherited)) continue; pkg=inherited[parts[2]] }
        if (name[parts[1]] && (pkg in packages)) edge[pkg,name[parts[1]]]=1
    }
    best=0
    for (pkg in packages) {
        span=longest(pkg)
        if (span>best || (span==best && path[pkg]<bestpath)) { best=span; bestpath=path[pkg] }
    }
    if (cycle) print "Rust 最长依赖链：含 dev/build 边出现环；按实际构建目标确认，不能给出 DAG 最长链。"
    else if (best) print "Rust 最长本地依赖链（上游 -> 下游）：" bestpath "（" best " 个包；含 dev/build 边）。"
    else print "Rust 最长依赖链：未解析出 package，请核对 workspace 字面成员。"
}
