#!/usr/bin/env python3
"""Do accepted causes of similar past incidents improve root-cause ranking? All 90 RCAEval
cases in one store (the store similar_eval.py builds), scored by `ograph chains --top 5` AC@1.

Part 1, leave-one-out. Every incident's true chain is accepted. Each incident is then ranked
with `precedent_weight` from a grid: the verdicts of all OTHER incidents count, whenever they
opened (`past_any_time`), and its own never does. The weight is chosen on rep 1 (the smallest
weight with the best AC@1 there) and reported on reps 2-3. The incidents whose most similar
judged incident has a different root cause are counted separately: there a precedent can only
mislead. The same grid is also run the way production sees it, earlier incidents only.

Part 2, across faults (added after Part 1's first run, as a harder test for harm). RCAEval
repeats every fault three times on the same service, so in Part 1 each incident has two twins.
Here only the incidents of half the fault kinds are accepted, and the incidents of the other
half are ranked, so no precedent is a twin. Both halves take a turn.

Keep rule, set before the first run: the weight ships only if held-out AC@1 beats the weight-0
baseline and no case is made wrong among those whose most similar incident misleads.

Usage: precedent_eval.py --bin target/release/ograph --cases CASES --out WORKDIR [--jobs 3]
"""
import argparse, concurrent.futures as cf, pathlib
from datetime import datetime
from similar_eval import merged, accept_true_chains

GRID = [0.0, 0.01, 0.02, 0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1.0]

class Bench:
    """One merged store and the incidents scored on it."""
    def __init__(self, labels, og, jobs, scored=None):
        self.labels, self.og, self.jobs = labels, og, jobs
        self.cids = sorted(scored if scored is not None else labels)
        self.tune = [c for c in self.cids if labels[c]["rep"] == 1]; self.held = [c for c in self.cids if labels[c]["rep"] != 1]

    def ranked(self, sets):
        """Every scored incident's ranked applications under the given correlation overrides."""
        flags = [x for k, v in sets.items() for x in ("--set", f"{k}={v}")]
        with cf.ThreadPoolExecutor(self.jobs) as ex:
            return dict(zip(self.cids, ex.map(lambda c: self.og("chains", self.labels[c]["incident"], "--top", "5", *flags, ok=(0, 1))["apps"], self.cids)))

    def hit(self, apps, c):
        return bool(apps[c]) and apps[c][0]["app"] == self.labels[c]["root"]

    def ac1(self, apps, of):
        return sum(self.hit(apps, c) for c in of) / max(len(of), 1)

    def grid(self, title, key, extra, base, mislead):
        """AC@1 per weight, then the weight rep 1 chooses and what it does to reps 2-3."""
        print(f"\n{title}")
        runs = {w: self.ranked({key: w, **extra}) for w in GRID}
        for w in GRID:
            apps = runs[w]
            worse = [c for c in self.cids if self.hit(base, c) and not self.hit(apps, c)]
            better = [c for c in self.cids if self.hit(apps, c) and not self.hit(base, c)]
            print(f"  {w:<5g} all {self.ac1(apps, self.cids):.3f}  rep 1 {self.ac1(apps, self.tune):.3f}  reps 2-3 {self.ac1(apps, self.held):.3f}  "
                  f"misleading {sum(self.hit(apps, c) for c in mislead)}/{len(mislead)}  fixed {len(better)}  broken {len(worse)} ({sum(c in mislead for c in worse)} of them misleading)")
        best = max(self.ac1(runs[w], self.tune) for w in GRID)
        chosen = min(w for w in GRID if self.ac1(runs[w], self.tune) == best)
        print(f"  chosen on rep 1: {key} = {chosen:g} (rep 1 AC@1 {best:.3f}); held-out reps 2-3 AC@1 {self.ac1(runs[chosen], self.held):.3f} against {self.ac1(base, self.held):.3f} at weight 0")
        self.changes(runs[chosen], base, f"at {chosen:g}")
        return runs, chosen

    def changes(self, apps, base, at):
        l = self.labels
        for what, was, now in [("fixed", apps, base), ("broken", base, apps)]:
            for c in [c for c in self.cids if self.hit(was, c) and not self.hit(now, c)]:
                print(f"    {what} {at}: {c} rep {l[c]['rep']} {l[c]['fault']} on {l[c]['root']}: rank 1 was {base[c][0]['app']}, is {apps[c][0]['app']}")

    def marks(self, title, apps, field):
        l = self.labels
        top = [(c, apps[c][0]) for c in self.cids if apps[c]]
        marked = [(c, r) for c, r in top if field in r]
        right = sum(l[c]["root"] == r["app"] for c, r in marked)
        rest = [(c, r) for c, r in top if field not in r]
        print(f"  {title}: {len(marked)} of {len(top)} rank-1 roots carry the mark; {right} of them are the true root, {len(marked) - right} are not. "
              f"Unmarked rank-1 roots: {sum(l[c]['root'] == r['app'] for c, r in rest)} of {len(rest)} are the true root")

    def misleading(self, any_time):
        """Incidents with no judged incident to compare, or whose most similar one has another root."""
        when = lambda t: datetime.fromisoformat(t.replace("Z", "+00:00"))
        opened = {i: when(at) for i, at in self.og("sparql", "SELECT ?id ?at WHERE { ?i inc:id ?id ; evt:at ?at }")["rows"]}
        out = []
        for c in self.cids:
            l = self.labels[c]
            sims = [s for s in self.og("similar", l["incident"], "--top", "1000", ok=(0, 1)) or [] if s["cause_apps"] and (any_time or when(s["at"]) < opened[l["incident"]])]
            if not sims or l["root"] not in sims[0]["cause_apps"]:
                out.append(c)
        return out

def store(a, name):
    # The store's own setting is leave-one-out, so `similar`, which takes no --set, answers that way.
    return merged(str(pathlib.Path(a.bin).resolve()), pathlib.Path(a.cases).resolve(), pathlib.Path(a.out).resolve() / name, "past_any_time = true\n")

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True); ap.add_argument("--out", required=True)
    ap.add_argument("--jobs", type=int, default=3)
    a = ap.parse_args()
    labels, og = store(a, "all")
    b = Bench(labels, og, a.jobs)
    twins = [sum(1 for o in labels.values() if (o["root"], o["fault"]) == (l["root"], l["fault"])) - 1 for l in labels.values()]
    print(f"{len(labels)} incidents in one store; every incident has {min(twins)} to {max(twins)} others with the same root service and fault")
    base = b.ranked({})
    print(f"no verdicts: AC@1 all {b.ac1(base, b.cids):.3f}, rep 1 {b.ac1(base, b.tune):.3f}, reps 2-3 {b.ac1(base, b.held):.3f}")

    accepted = accept_true_chains(labels, og)
    print(f"\nPART 1. accepted a true chain for {len(accepted)}/{len(labels)} incidents")
    unchanged = b.ranked({})
    print(f"with the verdicts stored and precedent_weight 0, every ranking equals the one without verdicts: {all([r['app'] for r in unchanged[c]] == [r['app'] for r in base[c]] for c in b.cids)}")
    chosen = {}
    for name, any_time in [("leave-one-out (all other incidents)", True), ("earlier incidents only", False)]:
        mislead = b.misleading(any_time)
        print(f"\n{name}: {len(mislead)} incidents are misleading (no judged incident to compare, or the most similar one has another root)")
        runs, chosen[any_time] = b.grid(f"precedent_weight, {name}: AC@1 per weight", "precedent_weight", {"past_any_time": str(any_time).lower()}, base, mislead)
        b.marks("'accepted before' mark at weight 0", runs[0.0], "precedent")

    w = chosen[True]
    print(f"\nPART 2. across faults, at the weight Part 1 chose leave-one-out ({w:g})")
    faults = sorted({l["fault"] for l in labels.values()})
    halves = [faults[: len(faults) // 2], faults[len(faults) // 2 :]]
    for judged, scored in [halves, halves[::-1]]:
        labels, og = store(a, "judged-" + "-".join(judged))
        b = Bench(labels, og, a.jobs, [c for c in labels if labels[c]["fault"] in scored])
        base = b.ranked({})
        accepted = accept_true_chains({c: l for c, l in labels.items() if l["fault"] in judged}, og)
        mislead = b.misleading(True)
        print(f"\naccepted the true chain of the {len(accepted)} incidents with faults {judged}; scoring the {len(b.cids)} with faults {scored}; {len(mislead)} of them are misleading")
        runs, _ = b.grid("precedent_weight across faults: AC@1 per weight", "precedent_weight", {"past_any_time": "true"}, base, mislead)
        print(f"  at the Part 1 weight {w:g}: AC@1 {b.ac1(runs[w], b.cids):.3f} against {b.ac1(base, b.cids):.3f} at weight 0; misleading {sum(b.hit(runs[w], c) for c in mislead)}/{len(mislead)} right against {sum(b.hit(base, c) for c in mislead)}/{len(mislead)}")
        b.changes(runs[w], base, f"at the Part 1 weight {w:g}")
        b.marks("'accepted before' mark at weight 0", runs[0.0], "precedent")

if __name__ == "__main__":
    main()
