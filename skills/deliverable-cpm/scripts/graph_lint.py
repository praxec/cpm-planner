#!/usr/bin/env python3
"""Lint a cpm-planner graph before plan.submit.

Input: a JSON file holding either {"deliverables": [...]} or a plan.submit request
({"arguments": {"graph": {...}}}). A prerequisite may be an id string or
{"id": ..., "consumes": ...}; rationale may also live in metadata.consumes[id].
Milestones are deliverables with metadata.milestone == true.

Exit code 0 = clean, 1 = findings. Read-only.
"""
import json, sys

def load(path):
    d = json.load(open(path))
    g = d.get("arguments", {}).get("graph", d)
    return g["deliverables"]

def main(path):
    ds = load(path)
    T = {d["id"]: d for d in ds}
    P, why = {}, {}
    for d in ds:
        ids = []
        for p in d.get("prerequisites", []):
            pid = p["id"] if isinstance(p, dict) else p
            ids.append(pid)
            c = (p.get("consumes") if isinstance(p, dict) else None) or d.get("metadata", {}).get("consumes", {}).get(pid)
            why[(d["id"], pid)] = c
        P[d["id"]] = set(ids)
    findings = []
    for i, ps in P.items():
        for p in ps:
            if p not in T:
                findings.append(f"unknown prerequisite: {i} <- {p}")
    if findings:
        print("\n".join(findings)); return 1
    # cycles
    state = {}
    def visit(i, path):
        if state.get(i) == 1:
            findings.append("cycle: " + " -> ".join(path + [i])); return
        if state.get(i) == 2: return
        state[i] = 1
        for p in P[i]: visit(p, path + [i])
        state[i] = 2
    for i in T: visit(i, [])
    if findings:
        print("\n".join(findings)); return 1
    anc = {}
    def A(i):
        if i not in anc:
            anc[i] = set()
            for p in P[i]: anc[i] |= A(p) | {p}
        return anc[i]
    for d, ps in P.items():
        for u in ps:
            if any(u in A(o) for o in ps if o != u):
                findings.append(f"redundant edge (implied by another path): {d} <- {u}")
            if not why.get((d, u)):
                findings.append(f"edge without rationale (what does {d} consume from {u}?)")
    ms = [i for i, d in T.items() if d.get("metadata", {}).get("milestone")]
    if not ms:
        findings.append("no milestones marked (metadata.milestone = true)")
    else:
        feeding = set(ms).union(*[A(m) for m in ms])
        for i in sorted(set(T) - feeding):
            findings.append(f"feeds no milestone (extra or missing milestone): {i}")
    for i, d in T.items():
        if not d.get("metadata", {}).get("artifact"):
            findings.append(f"no artifact stated (what can be inspected when {i} is done?)")
    own = {}
    for i, d in T.items():
        for f in d.get("owned_files", []): own.setdefault(f, []).append(i)
    for f, v in own.items():
        for a in v:
            for b in v:
                if a < b and a not in A(b) and b not in A(a):
                    findings.append(f"unordered deliverables share a file: {f} ({a}, {b})")
    print("\n".join(findings) if findings else "clean")
    return 1 if findings else 0

if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
