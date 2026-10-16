import json, glob, sys, statistics as st, collections
root=sys.argv[1]
res=collections.defaultdict(lambda: collections.defaultdict(list))
for d in sorted(glob.glob(root+"/*")):
    s=d.split("/")[-1].split("-",1)[1]
    for f in glob.glob(d+"/*.json"):
        if f.endswith("pyperf.json"): continue
        ev=json.load(open(f))
        if sum(1 for e in ev if e[0]=="loadBefore")<50: continue
        # samples: from DB.__init__ to end of tpc_finish
        idx=[i for i,e in enumerate(ev) if e[0]=="DB.__init__"]
        for k,i in enumerate(idx):
            j=next(j for j in range(i,len(ev)) if ev[j][0]=="tpc_finish")
            seg=ev[i+1:j+1]
            start=seg[0][1]; end=seg[-1][1]+seg[-1][2]
            agg=collections.defaultdict(float)
            for n,b,du in seg: agg[n]+=du
            tot=end-start
            r=res[s]
            r["total"].append(tot*1e3)
            for n in ("loadBefore","tpc_begin","store","tpc_vote","tpc_finish"):
                r[n].append(agg[n]*1e3)
            r["python and rest"].append((tot-sum(agg.values()))*1e3)
            r["n loads"].append(sum(1 for e in seg if e[0]=="loadBefore"))
for s,r in res.items():
    print(s, "samples", len(r["total"]))
    for k,v in r.items():
        print("   %-16s median %.2f  (min %.2f max %.2f)"%(k, st.median(v), min(v), max(v)))
