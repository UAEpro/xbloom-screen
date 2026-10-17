#!/usr/bin/env python3
"""Generate golden test vectors for the Rust ``xbloom-core`` crate from the Python package.

The Python package (``xbloom_ble``, at the repo root) is the reference implementation of the
reverse-engineered protocol — its frames are verified byte-for-byte against real HCI captures.
This script runs it over a broad set of inputs and writes the results to
``crates/xbloom-core/tests/golden/vectors.json``. The Rust tests then assert the port produces
**identical bytes** (frames, CRCs) and identical decodes (telemetry, validation, derived values).

Run from the repo root (CI does this and fails if the checked-in file drifts)::

    python3 screen/tools/gen_golden.py            # rewrite the vectors
    python3 screen/tools/gen_golden.py --check    # exit 1 if they would change

No personal data is involved: every input here is synthetic.
"""

from __future__ import annotations

import argparse
import json
import math
import struct
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from xbloom_ble.protocol import (  # noqa: E402
    build_cancel,
    build_commit,
    build_load_frames,
    build_save_slot,
    build_session_start,
    build_set_mode,
    build_start,
    build_status_query,
    crc16_kermit,
)
from xbloom_ble.recipe import Recipe, RecipeError  # noqa: E402
from xbloom_ble.telemetry import parse_notification  # noqa: E402

OUT = REPO / "screen" / "crates" / "xbloom-core" / "tests" / "golden" / "vectors.json"


def pour(ml, temp, pattern="spiral", agitation=False, pause=5, rpm=100, flow=3.0):
    return {"ml": ml, "temp": temp, "pattern": pattern, "agitation": agitation,
            "pause": pause, "rpm": rpm, "flow": flow}


# Protocol-level recipes (unvalidated wire inputs) covering every encoder path.
WIRE_RECIPES = {
    "simple": {"dose": 16, "grind": 62, "pours": [
        pour(35, 90, pause=40), pour(115, 90)]},
    "over_127ml": {"dose": 18, "grind": 75, "pours": [
        pour(45, 92, agitation=True, pause=30, rpm=90), pour(235, 91, rpm=120, flow=3.2)]},
    "center_and_ring": {"dose": 20, "grind": 50, "stage_temps": (108.0, 88.0), "pours": [
        pour(40, 93, agitation=True, pause=35, flow=2.8), pour(80, 90, "ring", pause=10, rpm=90),
        pour(60, 88, "center", pause=0, rpm=0)]},
    "no_grind": {"dose": 15, "grind": 0, "pours": [
        pour(50, 94, agitation=True, pause=45, rpm=110), pour(190, 92, rpm=110)]},
    "many_large": {"dose": 18, "grind": 40, "stage_temps": (110.0, 92.5), "pours": [
        pour(300, 95, pause=20), pour(254, 94, "ring", pause=0, rpm=60, flow=3.5),
        pour(127, 93, flow=3.1), pour(128, 92, "center", rpm=0)]},
    "pause_edges": {"dose": 12, "grind": 1, "pours": [
        pour(30, 40, pause=0, rpm=60), pour(150, 95, pause=255, flow=3.3),
        pour(1, 70, pause=99, flow=3.4)]},
    "odd_flows": {"dose": 16, "grind": 80, "pours": [
        pour(40, 90, flow=3.05), pour(60, 90, flow=3.15), pour(70, 90, flow=3.25),
        pour(70, 90, flow=3.35)]},
    "explicit_tail": {"dose": 16, "grind": 62, "tail": 0xA0, "pours": [
        pour(35, 90, pause=40), pour(115, 90)]},
    "custom_seq": {"dose": 16, "grind": 62, "seq": 0x20, "pours": [
        pour(35, 90, pause=40), pour(115, 90)]},
}


def frames_hex(frames):
    return [f.hex() for f in frames]


def gen_protocol():
    load = {name: frames_hex(build_load_frames(r)) for name, r in WIRE_RECIPES.items()}
    slots = []
    for name in ("simple", "over_127ml", "no_grind", "center_and_ring"):
        for slot in (0, 1, 2):
            for scale in (True, False):
                slots.append({"recipe": name, "slot": slot, "scale": scale,
                              "frame": build_save_slot(WIRE_RECIPES[name], slot, scale).hex()})
    return {
        "wire_recipes": {
            name: {**r, "stage_temps": list(r.get("stage_temps", (110.0, 90.0)))}
            for name, r in WIRE_RECIPES.items()
        },
        "load_frames": load,
        "save_slot": slots,
        "commit": build_commit().hex(),
        "start": build_start().hex(),
        "cancel": build_cancel().hex(),
        "session_start": build_session_start().hex(),
        "status_query": build_status_query().hex(),
        "set_mode_pro": build_set_mode(True).hex(),
        "set_mode_auto": build_set_mode(False).hex(),
    }


def gen_crc():
    cases = [b"", b"\x00", b"123456789", bytes(range(20)), bytes(range(256)),
             b"\xff" * 33, bytes.fromhex("580101421f0c00000001")]
    return [{"data": c.hex(), "crc": crc16_kermit(c)} for c in cases]


def notif(ftype: int, payload: bytes, sub: int = 0x00) -> bytes:
    """Build a machine→host ``58 02 07`` notification the way the firmware frames them."""
    body = bytearray([0x58, 0x02, 0x07, ftype, sub]) + b"\x00\x00\x00\x00" + b"\xc1" + payload
    body[5:9] = struct.pack("<I", len(body) + 2)
    return bytes(body) + struct.pack("<H", crc16_kermit(bytes(body)))


def gen_telemetry():
    frames = []
    for state in (0x01, 0x0C, 0x0F, 0x10, 0x1D, 0x1E, 0x1F, 0x22, 0x23, 0x24, 0x25, 0x3B,
                  0x41, 0x43, 0x77):
        frames.append(notif(0x57, bytes([state, 0x00, 0x00])))
    for mg in (0.0, 1234.5, 45000.0, 240000.0, -3.0, 3e9, float("nan")):
        frames.append(notif(0x4B, struct.pack("<f", mg)))
    for g in (0.0, 0.42, 16.0, 199.99, 1999.0, 2001.0, -0.5):
        frames.append(notif(0x15, struct.pack("<f", g)))
    for ack in (0xA4, 0xA6, 0xA8, 0x41, 0x44, 0x42, 0x46, 0x56, 0xF6, 0x49):
        frames.append(notif(ack, b"\x01\x02"))
    # Odd shapes: short, wrong magic, a scale frame truncated before its float,
    # an empty status payload, and a frame whose 0xc1 marker is not at offset 9.
    frames += [
        bytes.fromhex("5802"),
        bytes.fromhex("590207570000000000c101aaaa"),
        notif(0x4B, b"\x00\x00")[:12],
        notif(0x57, b""),
        bytes.fromhex("58020757000d00000000c11faabb"),
        bytes.fromhex("5802075700000000c11faabbcc"),
    ]
    out = []
    for f in frames:
        ev = parse_notification(f)
        exp = None
        if ev is not None:
            exp = {"state": ev.state, "state_name": ev.state_name,
                   "water_g": None if ev.water_g is None or math.isnan(ev.water_g) else ev.water_g,
                   "coffee_g": None if ev.coffee_g is None or math.isnan(ev.coffee_g)
                   else ev.coffee_g,
                   "is_terminal": ev.is_terminal}
        out.append({"frame": f.hex(), "event": exp})
    return out


def yp(ml, temp, pattern="spiral", **kw):
    return {"ml": ml, "temp_c": temp, "pattern": pattern, **kw}


VALIDATION_CASES = {
    "ok_basic": {"name": "Basic", "dose_g": 16, "grind": 62, "pours": [
        yp(45, 93, agitation=True, pause_s=40, rpm=100), yp(195, 91, pause_s=5, rpm=100)]},
    "ok_ratio": {"name": "Ratio", "dose_g": 16, "grind": 62, "ratio": 15, "pours": [
        yp(45, 93, rpm=100, pause_s=40), yp(100, 91, rpm=100), yp(95, 90, rpm=100)]},
    "ok_no_grind": {"name": "NG", "dose_g": 15, "grind": 0, "pours": [
        yp(50, 94, rpm=110), yp(190, 92, rpm=110)]},
    "ok_center_rpm0": {"name": "C", "dose_g": 10, "grind": 30, "pours": [
        yp(40, 90, rpm=60), yp(110, 90, "center", rpm=0, flow_ml_s=3.5)]},
    "ok_metadata": {"name": "Meta", "dose_g": 18, "grind": 58, "dripper": "Omni",
                    "kind": "custom", "water_ml": 300, "hot_water_ml": 180, "ice_g": 80,
                    "time": "~1:45", "note": "iced", "pours": [
                        yp(50, 94, rpm=110, label="Bloom"), yp(130, 92, rpm=100)]},
    "bad_one_pour": {"name": "x", "dose_g": 16, "grind": 62, "pours": [yp(240, 92, rpm=100)]},
    "bad_dose_high": {"name": "x", "dose_g": 19, "grind": 62, "pours": [
        yp(40, 92, rpm=100), yp(200, 92, rpm=100)]},
    "bad_dose_zero": {"name": "x", "dose_g": 0, "grind": 62, "pours": [
        yp(40, 92, rpm=100), yp(200, 92, rpm=100)]},
    "bad_grind": {"name": "x", "dose_g": 16, "grind": 81, "pours": [
        yp(40, 92, rpm=100), yp(200, 92, rpm=100)]},
    "bad_temp": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 96, rpm=100), yp(200, 39, rpm=100)]},
    "bad_rpm": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 92, rpm=50), yp(200, 92, rpm=130)]},
    "bad_center_rpm": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 92, rpm=100), yp(200, 92, "center", rpm=30)]},
    "bad_flow": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 92, rpm=100, flow_ml_s=2.9), yp(200, 92, rpm=100, flow_ml_s=3.6)]},
    "bad_pause": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 92, rpm=100, pause_s=256), yp(200, 92, rpm=100, pause_s=-1)]},
    "bad_agitation_ring": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 92, "ring", rpm=100, agitation=True), yp(200, 92, rpm=100)]},
    "bad_pattern": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(40, 92, "zigzag", rpm=100), yp(200, 92, rpm=100)]},
    "bad_ratio_sum": {"name": "x", "dose_g": 16, "grind": 62, "ratio": 15, "pours": [
        yp(40, 92, rpm=100), yp(100, 92, rpm=100)]},
    "bad_ml": {"name": "x", "dose_g": 16, "grind": 62, "pours": [
        yp(0, 92, rpm=100), yp(4001, 92, rpm=100)]},
    "bad_stage_temp": {"name": "x", "dose_g": 16, "grind": 62, "stage_temps": [131, 39],
                       "pours": [yp(40, 92, rpm=100), yp(200, 92, rpm=100)]},
    "bad_metadata": {"name": "x", "dose_g": 16, "grind": 62, "water_ml": -5, "pours": [
        yp(40, 92, rpm=100), yp(200, 92, rpm=100)]},
    "bad_missing_dose": {"name": "x", "grind": 62, "pours": [
        yp(40, 92, rpm=100), yp(200, 92, rpm=100)]},
    "bad_missing_pours": {"name": "x", "dose_g": 16, "grind": 62},
    "bad_empty_pours": {"name": "x", "dose_g": 16, "grind": 62, "pours": []},
}


def gen_recipes():
    out = []
    for name, data in VALIDATION_CASES.items():
        entry = {"name": name, "input": data}
        try:
            r = Recipe.from_dict(json.loads(json.dumps(data)))
        except RecipeError as exc:
            entry.update(valid=False, error=str(exc))
        else:
            entry.update(valid=True, total_water_ml=r.total_water_ml,
                         effective_ratio=r.effective_ratio,
                         load_frames=frames_hex(build_load_frames(r.to_protocol_dict())),
                         to_dict=r.to_dict())
        out.append(entry)
    # The bundled example recipes must round-trip and encode identically too.
    for path in sorted((REPO / "recipes").glob("*.yaml")):
        r = Recipe.from_yaml(path)
        out.append({"name": path.stem, "yaml": path.read_text(), "valid": True,
                    "total_water_ml": r.total_water_ml, "effective_ratio": r.effective_ratio,
                    "load_frames": frames_hex(build_load_frames(r.to_protocol_dict())),
                    "to_dict": r.to_dict()})
    return out


def build() -> str:
    data = {
        "_comment": "Generated by screen/tools/gen_golden.py from the xbloom_ble Python "
                    "reference. Do not edit by hand.",
        "crc": gen_crc(),
        "protocol": gen_protocol(),
        "telemetry": gen_telemetry(),
        "recipes": gen_recipes(),
    }
    return json.dumps(data, indent=1, sort_keys=False) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--check", action="store_true", help="fail if the vectors are out of date")
    args = ap.parse_args()
    text = build()
    if args.check:
        current = OUT.read_text() if OUT.exists() else ""
        if current != text:
            print(f"{OUT} is out of date — run screen/tools/gen_golden.py", file=sys.stderr)
            return 1
        print("golden vectors up to date")
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
