#!/usr/bin/env python3
"""Break-even model for fronting an Elasticsearch frozen-tier S3 repository with S4.

S4 saves storage but costs a host (the gateway is a separate line item not modeled
by the raw byte counts). This computes how large the frozen repository has to be
before the storage saving covers the host cost.

Model
-----
Monthly storage saving:   saved_ratio * size_TB * PRICE_PER_TB_MONTH   ($/mo)
Monthly host cost:        host_usd_month * instances                   ($/mo)

Break-even (net zero):
    saved_ratio * TB * PRICE_PER_TB_MONTH = host_usd_month * instances
=>  break_even_TB = (host_usd_month * instances) / (saved_ratio * PRICE_PER_TB_MONTH)

PRICE_PER_TB_MONTH defaults to 23 ($0.023/GB-month S3 Standard, us-east-1, * 1000).
`saved_ratio` is the *measured* repository-byte saving from this harness
(phase_a.json), NOT a codec micro-benchmark figure.

Everything here is arithmetic on the measured saved_ratio + an explicit,
parameterised host price. No new measurement is invented. Output is raw JSON to
stdout (and optionally a file) plus a human-readable summary on stderr.
"""
import argparse
import json
import sys

# Measured repository-byte savings from benches/.../results/phase_a.json
# (S4 zstd-3 vs direct, the default PUT codec). These are the authoritative
# 2026-06-18 numbers; keep them in sync with phase_a.json.
SAVED_RATIO = {
    "standard-default": 0.270,  # 1440.8 -> 1051.2 MB
    "best_compression": 0.148,  # 1057.6 ->  901.3 MB
    "logsdb": 0.222,            #  660.9 ->  514.3 MB
}
# zstd-19 via `s4 recompact` on the standard-default repo (phase_d evidence).
SAVED_RATIO_RECOMPACT = {
    "standard-default-zstd19": 0.332,  # 1440.8 -> 962.0 MB
}


def break_even_tb(host_usd_month, instances, saved_ratio, price_per_tb_month):
    denom = saved_ratio * price_per_tb_month
    if denom <= 0:
        return float("inf")
    return (host_usd_month * instances) / denom


def net_savings_per_month(size_tb, saved_ratio, price_per_tb_month,
                          host_usd_month, instances):
    storage_saved = saved_ratio * size_tb * price_per_tb_month
    host_cost = host_usd_month * instances
    return storage_saved - host_cost, storage_saved, host_cost


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--s4-host-usd-month", type=float, default=70.0,
                    help="Monthly cost of ONE S4 gateway host (default 70; "
                         "~a t3.large/c-family on-demand, parameterise to your fleet)")
    ap.add_argument("--instances", type=int, default=1,
                    help="S4 instances in the fleet (HA uses >=2 stateless instances "
                         "behind a load balancer; sidecars live in S3 so instances are "
                         "stateless). Default 1 (non-HA).")
    ap.add_argument("--price-per-tb-month", type=float, default=23.0,
                    help="S3 storage $/TB-month (default 23 = $0.023/GB-mo Standard)")
    ap.add_argument("--scales-tb", type=float, nargs="+", default=[50, 100, 500, 1000],
                    help="Repository sizes (TB) to report net savings for "
                         "(default 50 100 500 1000)")
    ap.add_argument("--out", help="Also write the JSON to this path")
    args = ap.parse_args()

    scenarios = {}
    all_ratios = {**SAVED_RATIO, **SAVED_RATIO_RECOMPACT}
    for name, ratio in all_ratios.items():
        be1 = break_even_tb(args.s4_host_usd_month, 1, ratio, args.price_per_tb_month)
        beN = break_even_tb(args.s4_host_usd_month, args.instances, ratio,
                            args.price_per_tb_month)
        scales = {}
        for tb in args.scales_tb:
            net1, saved1, host1 = net_savings_per_month(
                tb, ratio, args.price_per_tb_month, args.s4_host_usd_month, 1)
            netN, savedN, hostN = net_savings_per_month(
                tb, ratio, args.price_per_tb_month, args.s4_host_usd_month, args.instances)
            scales[f"{int(tb)}TB"] = {
                "non_ha_1_instance": {
                    "storage_saved_usd_mo": round(saved1, 2),
                    "host_cost_usd_mo": round(host1, 2),
                    "net_usd_mo": round(net1, 2),
                    "net_usd_yr": round(net1 * 12, 2),
                    "net_positive": net1 > 0,
                },
                f"ha_{args.instances}_instances": {
                    "storage_saved_usd_mo": round(savedN, 2),
                    "host_cost_usd_mo": round(hostN, 2),
                    "net_usd_mo": round(netN, 2),
                    "net_usd_yr": round(netN * 12, 2),
                    "net_positive": netN > 0,
                },
            }
        scenarios[name] = {
            "saved_ratio": ratio,
            "break_even_TB_non_ha_1_instance": round(be1, 2),
            f"break_even_TB_ha_{args.instances}_instances": round(beN, 2),
            "scales": scales,
        }

    out = {
        "model": "break_even_TB = host_usd_month * instances / (saved_ratio * price_per_TB_month)",
        "params": {
            "s4_host_usd_month": args.s4_host_usd_month,
            "instances": args.instances,
            "price_per_tb_month": args.price_per_tb_month,
            "scales_tb": args.scales_tb,
        },
        "source": "saved_ratio from benches/elasticsearch-frozen/results/phase_a.json "
                  "(2026-06-18); host price is the parameterised --s4-host-usd-month",
        "scenarios": scenarios,
    }

    js = json.dumps(out, indent=2)
    print(js)
    if args.out:
        with open(args.out, "w") as f:
            f.write(js + "\n")

    # Human summary on stderr (doesn't pollute the JSON on stdout).
    def fmt(name, sc):
        be1 = sc["break_even_TB_non_ha_1_instance"]
        beN = sc[f"break_even_TB_ha_{args.instances}_instances"]
        print(f"  {name:26s} saved={sc['saved_ratio']*100:4.1f}%  "
              f"break-even: {be1:6.2f} TB (1 inst) / {beN:6.2f} TB ({args.instances} inst)",
              file=sys.stderr)
    print(f"\nbreak-even @ ${args.s4_host_usd_month:.0f}/mo per host, "
          f"${args.price_per_tb_month:.0f}/TB-mo S3:", file=sys.stderr)
    for name, sc in scenarios.items():
        fmt(name, sc)
    print("\nnet savings (HA = {} instances):".format(args.instances), file=sys.stderr)
    for name, sc in scenarios.items():
        for scale in ("500TB", "1000TB"):
            if scale in sc["scales"]:
                ha = sc["scales"][scale][f"ha_{args.instances}_instances"]
                print(f"  {name:26s} {scale:>7}: ${ha['net_usd_mo']:>10,.0f}/mo "
                      f"= ${ha['net_usd_yr']:>12,.0f}/yr net "
                      f"({'positive' if ha['net_positive'] else 'NEGATIVE'})",
                      file=sys.stderr)


if __name__ == "__main__":
    main()
