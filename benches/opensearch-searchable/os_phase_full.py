#!/usr/bin/env python3
"""OpenSearch full benchmark through S4 (now that --logical-etag unblocks the
repository-s3 write path). Per index codec x repo: snapshot (timed), measure
real stored bytes on MinIO, then searchable-snapshot (remote_snapshot) cold
query latency direct vs S4."""
import json, time, subprocess, urllib.request, os, statistics

ES="http://localhost:9200"; MINIO="http://localhost:9000"
ENV=dict(os.environ, AWS_ACCESS_KEY_ID="minioadmin", AWS_SECRET_ACCESS_KEY="minioadmin",
         AWS_REGION="us-east-1", AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
         AWS_RESPONSE_CHECKSUM_VALIDATION="when_required")
CODECS=["os-default","os-bestcomp","os-zstd","os-zstdnodict"]
REPOS=[("osr_direct","default","os-repo-direct","direct"),
       ("osr_s4z3","s4z3","os-repo-s4z3","S4 zstd-3"),
       ("osr_s4z9","s4z9","os-repo-s4z9","S4 zstd-9")]

def es(method,path,body=None):
    data=json.dumps(body).encode() if body is not None else None
    req=urllib.request.Request(f"{ES}{path}",data=data,headers={"Content-Type":"application/json"},method=method)
    try:
        with urllib.request.urlopen(req) as r: return json.load(r)
    except urllib.error.HTTPError as e: return {"_error":e.code,"_body":e.read().decode()[:200]}

def aws(*a): return subprocess.run(["aws","--endpoint-url",MINIO,*a],env=ENV,capture_output=True,text=True)
def bucket_bytes(b):
    r=aws("s3api","list-objects-v2","--bucket",b,"--query","sum(Contents[].Size)","--output","text")
    try: return int(float(r.stdout.strip()))
    except Exception: return 0

# ---- cost + snapshot throughput (4 codecs x 3 repos) ----
cost=[]
for ix in CODECS:
    for repo,client,bucket,label in REPOS:
        es("DELETE",f"/_snapshot/{repo}"); aws("s3","rm",f"s3://{bucket}","--recursive")
        es("PUT",f"/_snapshot/{repo}",{"type":"s3","settings":{"bucket":bucket,"client":client,"max_snapshot_bytes_per_sec":"-1"}})
        t0=time.time()
        r=es("PUT",f"/_snapshot/{repo}/snap?wait_for_completion=true",{"indices":ix,"include_global_state":False})
        wall=time.time()-t0
        state=r.get("snapshot",{}).get("state","?")
        st=es("GET",f"/_snapshot/{repo}/snap/_status")["snapshots"][0]["stats"]
        es_bytes=st["total"]["size_in_bytes"]
        stored=bucket_bytes(bucket)
        cost.append({"codec":ix,"repo":label,"state":state,"es_bytes":es_bytes,"stored_bytes":stored,"wall_s":round(wall,2)})
        print(f"{ix:14s} {label:10s} state={state:8s} es={es_bytes/1e6:7.1f}MB stored={stored/1e6:7.1f}MB wall={wall:6.2f}s",flush=True)
        es("DELETE",f"/_snapshot/{repo}/snap")

# ---- searchable-snapshot cold search (direct vs S4 zstd-3) ----
QUERIES={
 "term(status:500)":{"size":0,"track_total_hits":True,"query":{"term":{"http.response.status_code":500}}},
 "agg(date_hist+terms)":{"size":0,"aggs":{"svc":{"terms":{"field":"service.name","size":10}},"t":{"date_histogram":{"field":"@timestamp","fixed_interval":"1h"}}}},
 "fulltext(message:items)":{"size":0,"track_total_hits":True,"query":{"match":{"message":"items"}}},
}
search=[]
for ix in CODECS:
    for repo,client,bucket,label in [("osr_direct","default","os-repo-direct","direct"),("osr_s4z3","s4z3","os-repo-s4z3","S4 zstd-3")]:
        es("DELETE",f"/_snapshot/{repo}"); aws("s3","rm",f"s3://{bucket}","--recursive")
        es("PUT",f"/_snapshot/{repo}",{"type":"s3","settings":{"bucket":bucket,"client":client,"max_snapshot_bytes_per_sec":"-1"}})
        es("PUT",f"/_snapshot/{repo}/snb?wait_for_completion=true",{"indices":ix,"include_global_state":False})
        frozen=f"rs-{label.replace(' ','').replace('-','').lower()}-{ix}"
        es("DELETE",f"/{frozen}")
        m=es("POST",f"/_snapshot/{repo}/snb/_restore?wait_for_completion=true",
             {"indices":ix,"storage_type":"remote_snapshot","rename_pattern":ix,"rename_replacement":frozen,
              "index_settings":{"index.number_of_replicas":0}})
        if "_error" in m: print(f"MOUNT FAIL {ix} {label}: {m.get('_body')}",flush=True); continue
        for _ in range(20):
            h=es("GET",f"/_cluster/health/{frozen}?wait_for_status=yellow&timeout=5s")
            if h.get("status") in ("yellow","green"): break
            time.sleep(1)
        for qn,qb in QUERIES.items():
            # cold-ish: clear caches then query (remote_snapshot has its own file cache; clear what we can)
            es("POST",f"/{frozen}/_cache/clear")
            took=[]
            for _ in range(4):
                es("POST",f"/{frozen}/_cache/clear")
                r=es("GET",f"/{frozen}/_search",qb)
                if isinstance(r,dict) and "took" in r: took.append(r["took"])
            rec={"codec":ix,"repo":label,"query":qn,"med_ms":round(statistics.median(took),1) if took else None}
            search.append(rec)
            print(f"{ix:14s} {label:10s} {qn:24s} med={rec['med_ms']}",flush=True)
        es("DELETE",f"/{frozen}")

json.dump({"cost":cost,"search":search},open("./results/os_full.json","w"),indent=2)
print("wrote results/os_full.json")
