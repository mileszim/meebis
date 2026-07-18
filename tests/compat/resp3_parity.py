#!/usr/bin/env python3
"""RESP3 parity check: run the same operations against meebis and a reference
redis-server using redis-py (which negotiates RESP3 by default) and assert the
decoded Python results match.

Usage: python3 resp3_parity.py <meebis_port> <redis_port>
"""
import sys

import redis


def run(port):
    r = redis.Redis(port=port, decode_responses=True)  # default protocol -> RESP3
    o = {}
    r.flushall()

    r.set("py", "works", ex=100)
    o["get"] = r.get("py")
    o["ttl_ok"] = 90 <= r.ttl("py") <= 100

    r.rpush("l", "a", "b", "c")
    o["lrange"] = r.lrange("l", 0, -1)

    r.hset("h", mapping={"a": "1", "b": "2", "c": "3"})
    o["hgetall"] = sorted(r.hgetall("h").items())

    r.sadd("s", "x", "y", "z")
    o["smembers"] = sorted(r.smembers("s"))
    o["sismember"] = r.sismember("s", "x")

    r.zadd("z", {"x": 1, "y": 2, "w": 1.5})
    o["zrange_ws"] = r.zrange("z", 0, -1, withscores=True)
    o["zscore"] = r.zscore("z", "y")
    o["zrangebyscore_ws"] = r.zrangebyscore("z", "-inf", "+inf", withscores=True)
    o["zpopmin"] = r.zpopmin("z")
    o["zpopmax"] = r.zpopmax("z", 2)

    o["get_missing"] = r.get("nope")
    o["exists"] = r.exists("l")
    o["type"] = r.type("l")
    o["config"] = r.config_get("maxmemory")
    o["object_encoding"] = r.object("encoding", "l")

    p = r.pipeline()
    p.incr("c"); p.incr("c"); p.get("c")
    o["pipeline"] = p.execute()

    with r.pipeline() as pipe:
        pipe.watch("t")
        pipe.multi()
        pipe.set("t", "5")
        pipe.incr("t")
        o["transaction"] = pipe.execute()

    # Random commands: compare structure/shape rather than exact values.
    r.hset("hh", mapping={"a": "1", "b": "2", "c": "3"})
    hv = r.hrandfield("hh", 3, withvalues=True)
    o["hrandfield_shape"] = (all(isinstance(p, (list, tuple)) and len(p) == 2 for p in hv), len(hv))

    r.zadd("zz", {"a": 1, "b": 2, "c": 3})
    zv = r.zrandmember("zz", 3, withscores=True)
    o["zrandmember_shape"] = (all(isinstance(p, (list, tuple)) and len(p) == 2 for p in zv), len(zv))

    return o


def main():
    if len(sys.argv) != 3:
        print("usage: resp3_parity.py <meebis_port> <redis_port>", file=sys.stderr)
        return 2
    mport, rport = int(sys.argv[1]), int(sys.argv[2])
    meebis = run(mport)
    reference = run(rport)

    ok = True
    for key in sorted(set(meebis) | set(reference)):
        if meebis.get(key) != reference.get(key):
            ok = False
            print(f"    MISMATCH {key}: meebis={meebis.get(key)!r} redis={reference.get(key)!r}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
