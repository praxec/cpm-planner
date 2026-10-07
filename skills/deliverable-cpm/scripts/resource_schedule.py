#!/usr/bin/env python3
"""Resource-levelled schedule for a cpm-planner graph (list scheduling).

Usage: resource_schedule.py GRAPH.json '{"agent":1,"worker":5,"owner":1}' [resource_key]
resource_key defaults to metadata.owner. Priority = longest remaining tail
(classic CPM-aware list scheduling). Prints makespan, per-resource load and the
driving chain (dependency or resource waits). Read-only.
"""
import heapq, json, sys

def main(path, caps, key="owner"):
    d = json.load(open(path)); ds = d.get("arguments", {}).get("graph", d)["deliverables"]
    T = {x["id"]: x for x in ds}
    P = {i: [p["id"] if isinstance(p, dict) else p for p in T[i].get("prerequisites", [])] for i in T}
    H = {i: float(T[i].get("estimated_effort_hours", 0)) for i in T}
    R = {i: T[i].get("metadata", {}).get(key, "unassigned") for i in T}
    missing = sorted({R[i] for i in T} - set(caps))
    if missing: sys.exit(f"no capacity given for resources: {missing}")
    S = {i: [] for i in T}
    for i in T:
        for p in P[i]: S[p].append(i)
    tail = {}
    def L(i):
        if i not in tail: tail[i] = H[i] + max([L(s) for s in S[i]], default=0)
        return tail[i]
    slots = {r: [0.0] * n for r, n in caps.items()}
    start, end, todo, running, t = {}, {}, set(T), [], 0.0
    while todo or running:
        for i in sorted([i for i in todo if all(p in end for p in P[i])], key=lambda i: -L(i)):
            k = min(range(len(slots[R[i]])), key=lambda j: slots[R[i]][j])
            if slots[R[i]][k] <= t:
                start[i] = t; slots[R[i]][k] = t + H[i]; heapq.heappush(running, (t + H[i], i)); todo.discard(i)
        if running:
            e, i = heapq.heappop(running); t = max(t, e); end[i] = e
        else:
            t += 0.25
    load = {r: sum(H[i] for i in T if R[i] == r) for r in caps}
    last = max(end, key=end.get); chain = [last]
    while True:
        i = chain[-1]
        nxt = [p for p in P[i] if abs(end[p] - start[i]) < 1e-6] or \
              [j for j in T if R[j] == R[i] and j not in chain and abs(end[j] - start[i]) < 1e-6]
        if not nxt: break
        chain.append(nxt[0])
    print(f"makespan {max(end.values()):.1f} h")
    print("load:", {r: f"{load[r]:.1f}h/{caps[r]}" for r in caps})
    print("driving chain:", " <- ".join(chain))

if __name__ == "__main__":
    main(sys.argv[1], json.loads(sys.argv[2]), *(sys.argv[3:4]))
