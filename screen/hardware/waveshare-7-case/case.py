#!/usr/bin/env python3
"""A 3D-printable back case and desk stand for the Waveshare ESP32-S3-Touch-LCD-7.

The board ships as a bare cover glass with the LCD and the driver board on its back. This
script builds three parts with CadQuery and writes them as STL (oriented for printing) and
STEP (for editing in other CAD tools):

* ``back-case``: a shell that the cover glass drops into, held by four M3 × 12 screws in
  the brass inserts on the LCD's back. A window in the left side wall (seen from the back)
  lines up with both USB-C ports, so a straight USB-C cable plugs in with the case closed.
* ``stand``: a cradle that holds the case at a fixed angle. Gravity keeps the case in it;
  lift the screen out to unplug it or press RESET / BOOT.
* ``fit-test``: the USB end of the case only (about an hour to print), to check the fit
  of the glass, the two screws on that side and the cable before printing the whole case.

Usage::

    pip install cadquery
    python3 case.py                      # writes stl/ and step/ next to this file
    python3 case.py --out /tmp/case      # somewhere else
    python3 case.py --check board.stp    # also check clearances against Waveshare's 3D
                                         # model of the driver board (see the README)

Every dimension of the screen comes from Waveshare's drawings and 3D model (Rev 1.2); the
README says which. Change the parameters below to fit a different revision or printer.

Coordinates (the "device" frame, mm): the origin is the centre of the cover glass. X runs to
the right and Y up **as seen from the back**, so the USB-C ports are on the left and the
panel's flex cables fold over the top edge. Z runs from the glass front (0) towards the
back.
"""

from __future__ import annotations

import argparse
import math
import sys
from pathlib import Path

import cadquery as cq

# ── The screen ─────────────────────────────────────────────────────────────────────────

GLASS_W, GLASS_H, GLASS_R = 192.96, 110.76, 7.8  # cover glass outline (±0.1, drawing)
# The glass thickness isn't on the drawing (only glass + LCD = 5.0). The case is made for
# the thinnest likely glass: a thicker one just sits a little proud of the frame and moves
# the screw tips further from the LCD, never closer.
GLASS_T = 0.7
LCD_X = (-81.33, 84.39)  # the LCD module behind the glass
LCD_Y = (-47.44, 50.16)
LCD_BACK_Z = 5.0  # "TP + LCD = 5.00"
# The four M3 brass inserts in white blocks on the LCD's back: 126.20 × 65.65 apart.
INSERTS = [(x, y) for x in (-61.57, 64.63) for y in (-33.80, 31.85)]
INSERT_TOP_Z = 8.3  # glass front → top of the inserts. Measure yours (see the README).
# The driver board: Waveshare's STEP model, moved into the device frame.
PCB_OFFSET = (-55.04, -47.30, 10.1)  # where the model's origin lands (its top face is z = 0)
PCB_X = (PCB_OFFSET[0], PCB_OFFSET[0] + 106.01)
PCB_Y = (PCB_OFFSET[1], PCB_OFFSET[1] + 73.01)
# The two USB-C receptacles face -X at the board's left edge.
PORT_MOUTH_X = -55.02
PORT_Z = 11.27
PORTS = {"UART1": 3.53, "USB": -11.57}  # Y of each port's centre
# RESET and BOOT are side-push buttons on the board's bottom edge.
BUTTONS = {"RESET": -6.94, "BOOT": 4.16}  # X
BUTTON_Z = 10.2
BUTTON_TIP_Y = -47.96
TALLEST_Z = 18.4  # the upright PH2.0 headers
FLEX_X = (-25.0, 25.0)  # the panel and touch flex cables fold over the top edge here

# ── The case ───────────────────────────────────────────────────────────────────────────

FRAME = 2.0  # the case frame around the glass edge, seen from the front
POCKET_CLEAR = 0.25  # gap between the glass edge and the frame, per side
RIM = 2.4  # how far the ledge under the glass reaches in from the glass edge
RIM_RELIEF = 0.8  # the inner 0.8 mm of the ledge steps back by 0.8 mm (clears glue, tape)
BACK_T = 2.4  # back plate thickness
BACK_CLEAR = 1.6  # air above the tallest part
BACK_IN_Z = TALLEST_Z + BACK_CLEAR
BACK_OUT_Z = BACK_IN_Z + BACK_T
OUTER_W = GLASS_W + 2 * (POCKET_CLEAR + FRAME)
OUTER_H = GLASS_H + 2 * (POCKET_CLEAR + FRAME)
OUTER_R = GLASS_R + POCKET_CLEAR + FRAME
BACK_EDGE_CHAMFER = 1.5  # 45°, so the edge on the print bed needs no support
FRONT_EDGE_CHAMFER = 0.6

# Screw posts. The screw head sits deep in the post, so an M3 × 12 reaches only SCREW_ENGAGE
# into the insert and its tip stays clear of the LCD's back (checked below).
SCREW_LEN = 12.0
SCREW_ENGAGE = 2.2
SCREW_HOLE_D = 3.5
HEAD_BORE_D = 6.8  # M3 socket head (5.5 mm) or button head (5.7 mm)
POST_D = 10.0
POST_LOW_D = 7.0  # the post narrows near the driver board
POST_STEP_Z = 13.0
POST_GAP = 1.0  # between the post's end and the insert
POST_END_Z = INSERT_TOP_Z + POST_GAP
SEAT_Z = INSERT_TOP_Z + SCREW_LEN - SCREW_ENGAGE
# A thin skin across the bottom of each head bore, so the slicer can bridge it; the screw
# (or a 3 mm drill) pokes through it.
SKIN = 0.3

# The cable window: big enough for a USB-C plug's moulded body (up to 15 × 9.5 mm).
WINDOW_MARGIN_Y = 8.5
WINDOW_HALF_Z = 6.0
WINDOW_CHAMFER = 1.5  # chamfered, not rounded: prints as a plain bridge

# Vents in the upper half of the back: warm air leaves at the top, the stand doesn't cover
# them, and on the stand the back faces down, so nothing drips in.
VENT_COUNT = 9
VENT_PITCH = 6.0
VENT_W = 3.0
VENT_L = 36.0
VENT_CENTER = (-2.0, 15.0)

BUTTON_HOLE_D = 2.2  # pin holes for RESET and BOOT in the bottom wall

FLEX_RELIEF = 1.0  # extra room in the top wall where the flex cables fold
FLEX_RELIEF_Z = 8.0

# ── The stand ──────────────────────────────────────────────────────────────────────────

STAND_ANGLE = 65.0  # the screen's tilt, degrees from the table
STAND_W = 120.0
STAND_CLEAR = 0.8  # slot width over the case's thickness
STAND_FLOOR = 4.0  # material under the case's lowest corner
STAND_LIP_H = 9.0  # front lip, measured up the glass (the black border is 11 mm)
STAND_LIP_T = 4.0
STAND_BACK_H = 50.0  # back support, measured up the case
STAND_BACK_T = 5.0
STAND_BASE_T = 4.0
STAND_HEEL = 30.0  # how far the base reaches behind the case's centre of mass
FOOT_D = 10.5  # recesses for stick-on rubber feet
FOOT_DEPTH = 1.0

# ── helpers ────────────────────────────────────────────────────────────────────────────


def rounded_box(
    w: float, h: float, r: float, z0: float, z1: float, x: float = 0.0, y: float = 0.0
) -> cq.Solid:
    """A box from z0 to z1 with rounded vertical edges, centred on (x, y)."""
    wp = cq.Workplane("XY").workplane(offset=z0).center(x, y).rect(w, h).extrude(z1 - z0)
    return wp.edges("|Z").fillet(r).val()


def box(x0: float, x1: float, y0: float, y1: float, z0: float, z1: float) -> cq.Solid:
    return cq.Solid.makeBox(x1 - x0, y1 - y0, z1 - z0, pnt=cq.Vector(x0, y0, z0))


def cylinder_z(x: float, y: float, d: float, z0: float, z1: float) -> cq.Solid:
    return cq.Solid.makeCylinder(d / 2, z1 - z0, pnt=cq.Vector(x, y, z0), dir=cq.Vector(0, 0, 1))


def cylinder_y(x: float, z: float, d: float, y0: float, y1: float) -> cq.Solid:
    return cq.Solid.makeCylinder(d / 2, y1 - y0, pnt=cq.Vector(x, y0, z), dir=cq.Vector(0, 1, 0))


def window_bounds() -> tuple[float, float, float, float]:
    ys = list(PORTS.values())
    return (
        min(ys) - WINDOW_MARGIN_Y,
        max(ys) + WINDOW_MARGIN_Y,
        PORT_Z - WINDOW_HALF_Z,
        PORT_Z + WINDOW_HALF_Z,
    )


# ── parts ──────────────────────────────────────────────────────────────────────────────


def back_case() -> cq.Shape:
    """The back case in the device frame (front face at z = 0)."""
    shell = (
        cq.Workplane("XY")
        .rect(OUTER_W, OUTER_H)
        .extrude(BACK_OUT_Z)
        .edges("|Z")
        .fillet(OUTER_R)
        .faces(">Z")
        .edges()
        .chamfer(BACK_EDGE_CHAMFER)
        .faces("<Z")
        .edges()
        .chamfer(FRONT_EDGE_CHAMFER)
        .val()
    )
    # The glass pocket, the relief at the ledge's inner edge, and the cavity.
    pocket = rounded_box(
        GLASS_W + 2 * POCKET_CLEAR, GLASS_H + 2 * POCKET_CLEAR, GLASS_R + POCKET_CLEAR, -1, GLASS_T
    )
    inner = RIM - RIM_RELIEF
    relief = rounded_box(
        GLASS_W - 2 * inner,
        GLASS_H - 2 * inner,
        GLASS_R - inner,
        GLASS_T - 0.01,
        GLASS_T + RIM_RELIEF,
    )
    cavity = rounded_box(
        GLASS_W - 2 * RIM, GLASS_H - 2 * RIM, GLASS_R - RIM, GLASS_T - 0.01, BACK_IN_Z
    )
    case = shell.cut(pocket).cut(relief).cut(cavity)

    # Room for the flex cables where they fold over the top edge of the LCD.
    top = GLASS_H / 2 - RIM
    case = case.cut(
        box(FLEX_X[0], FLEX_X[1], top - 0.01, top + FLEX_RELIEF, GLASS_T, FLEX_RELIEF_Z)
    )

    # Screw posts.
    for x, y in INSERTS:
        post = cylinder_z(x, y, POST_D, POST_STEP_Z, BACK_IN_Z + 0.5).fuse(
            cylinder_z(x, y, POST_LOW_D, POST_END_Z, POST_STEP_Z + 0.01)
        )
        case = case.fuse(post)
    for x, y in INSERTS:
        case = case.cut(cylinder_z(x, y, SCREW_HOLE_D, POST_END_Z - 1, SEAT_Z - SKIN))
        case = case.cut(cylinder_z(x, y, HEAD_BORE_D, SEAT_Z, BACK_OUT_Z + 1))

    # The cable window in the left wall, lined up with both USB-C ports.
    y0, y1, z0, z1 = window_bounds()
    left = -OUTER_W / 2
    window = (
        cq.Workplane("YZ")
        .workplane(offset=left - 1)
        .center((y0 + y1) / 2, (z0 + z1) / 2)
        .rect(y1 - y0, z1 - z0)
        .extrude(RIM + POCKET_CLEAR + FRAME + 3)
        .edges("|X")
        .chamfer(WINDOW_CHAMFER)
        .val()
    )
    case = case.cut(window)

    # Vents in the upper half of the back.
    cx, cy = VENT_CENTER
    for i in range(VENT_COUNT):
        x = cx + (i - (VENT_COUNT - 1) / 2) * VENT_PITCH
        slot = (
            cq.Workplane("XY")
            .workplane(offset=BACK_IN_Z - 1)
            .center(x, cy)
            .slot2D(VENT_L, VENT_W, angle=90)
            .extrude(BACK_T + 2)
            .val()
        )
        case = case.cut(slot)

    # Pin holes for RESET and BOOT in the bottom wall.
    for x in BUTTONS.values():
        case = case.cut(
            cylinder_y(x, BUTTON_Z, BUTTON_HOLE_D, -OUTER_H / 2 - 1, -GLASS_H / 2 + RIM + 1)
        )
    return case.clean()


def fit_test(case: cq.Shape) -> cq.Shape:
    """The USB end of the case: the left screw posts, the cable window and a strip of the
    glass pocket."""
    x_cut = min(x for x, _ in INSERTS) + 22.0
    return case.intersect(box(-OUTER_W, x_cut, -OUTER_H, OUTER_H, -1, BACK_OUT_Z + 1)).clean()


def stand_geometry() -> dict[str, tuple[float, float]]:
    """Key points of the stand's side profile, (y, z): y towards the back, z up."""
    a = math.radians(STAND_ANGLE)
    u = (math.cos(a), math.sin(a))  # up the case
    n = (-math.sin(a), math.cos(a))  # out of the glass, towards the viewer
    thick = BACK_OUT_Z
    c = STAND_CLEAR / 2
    # The case's front-bottom corner F: the back-bottom corner is the lowest point.
    fz = STAND_FLOOR + (thick + c) * math.cos(a)
    f = (0.0, fz)

    def p(along: float, out: float) -> tuple[float, float]:
        return (f[0] + along * u[0] + out * n[0], f[1] + along * u[1] + out * n[1])

    def down_to(z: float, out: float) -> tuple[float, float]:
        """Where the line `out` (parallel to the case) crosses height z."""
        along = (z - f[1] - out * n[1]) / u[1]
        return p(along, out)

    front, back = c + STAND_LIP_T, -(thick + c + STAND_BACK_T)
    centre = p(OUTER_H / 2, -thick / 2)
    pts = {
        "toe": down_to(0, front),
        "lip_front": p(STAND_LIP_H, front),
        "lip_back": p(STAND_LIP_H, c),
        "slot_front": p(0, c),
        "slot_back": p(0, -(thick + c)),
        "support_front": p(STAND_BACK_H, -(thick + c)),
        "support_back": p(STAND_BACK_H, back),
        "support_base": down_to(STAND_BASE_T, back),
        "centre": centre,
    }
    heel_y = max(pts["support_base"][0] + 10, centre[0] + STAND_HEEL)
    pts["heel_top"] = (heel_y, STAND_BASE_T)
    pts["heel"] = (heel_y, 0.0)
    pts["u"], pts["n"], pts["f"] = u, n, f
    return pts


def stand() -> cq.Shape:
    """The desk stand, base on z = 0, its width along X."""
    g = stand_geometry()
    outline = [
        g[k]
        for k in (
            "toe",
            "lip_front",
            "lip_back",
            "slot_front",
            "slot_back",
            "support_front",
            "support_back",
            "support_base",
            "heel_top",
            "heel",
        )
    ]
    body = (
        cq.Workplane("YZ")
        .polyline(outline)
        .close()
        .extrude(STAND_W)
        .translate((-STAND_W / 2, 0, 0))
    )
    try:
        body = body.edges("|X").fillet(1.0)
    except Exception:  # a fillet that fails just leaves sharp edges
        pass
    solid = body.val()

    # Recesses for rubber feet.
    toe_y, heel_y = g["toe"][0], g["heel"][0]
    for x in (-STAND_W / 2 + 12, STAND_W / 2 - 12):
        for y in (toe_y + 10, heel_y - 10):
            solid = solid.cut(cylinder_z(x, y, FOOT_D, -1, FOOT_DEPTH))
    return solid.clean()


def for_stand(part: cq.Shape, g: dict) -> cq.Shape:
    """A part of the screen or case, moved from the device frame to where it sits in the
    stand: the case's bottom edge on the slot floor, the glass facing the viewer."""
    # Seen from the front, the device frame's X points left: turn the case round (180° about
    # Y), then tilt it back so its up (+Y) runs along the stand's u.
    moved = part.translate(cq.Vector(0, OUTER_H / 2, 0))  # bottom edge at y = 0, front at z = 0
    moved = moved.rotate(cq.Vector(0, 0, 0), cq.Vector(0, 1, 0), 180)
    moved = moved.rotate(cq.Vector(0, 0, 0), cq.Vector(1, 0, 0), STAND_ANGLE)
    (fy, fz), u = g["f"], g["u"]
    return moved.translate(cq.Vector(0, fy + 0.01 * u[0], fz + 0.01 * u[1]))


def for_printing(case: cq.Shape) -> cq.Shape:
    """Turn a case part over so its back lies on the print bed."""
    return case.rotate(cq.Vector(0, 0, 0), cq.Vector(1, 0, 0), 180).translate(
        cq.Vector(0, 0, BACK_OUT_Z)
    )


# ── checks ─────────────────────────────────────────────────────────────────────────────


def check(case: cq.Shape, stand_part: cq.Shape, board_step: Path | None) -> list[str]:
    """Clearance checks. Returns a list of problems (empty when everything fits)."""
    problems: list[str] = []
    info: list[str] = []

    def overlap(a: cq.Shape, b: cq.Shape) -> float:
        bb1, bb2 = a.BoundingBox(), b.BoundingBox()
        if bb1.xmin > bb2.xmax or bb2.xmin > bb1.xmax or bb1.ymin > bb2.ymax or bb2.ymin > bb1.ymax:
            return 0.0
        if bb1.zmin > bb2.zmax or bb2.zmin > bb1.zmax:
            return 0.0
        return a.intersect(b).Volume()

    def gap(a: cq.Shape, b: cq.Shape) -> float:
        return a.distance(b)

    # The screen itself, simplified: the glass, the LCD, the insert blocks.
    glass = rounded_box(GLASS_W, GLASS_H, GLASS_R, 0, GLASS_T)
    lcd = box(LCD_X[0], LCD_X[1], LCD_Y[0], LCD_Y[1], GLASS_T, LCD_BACK_Z)
    blocks = [box(x - 6, x + 6, y - 3.5, y + 3.5, LCD_BACK_Z, INSERT_TOP_Z) for x, y in INSERTS]
    for name, part in [("glass", glass), ("LCD", lcd)] + [
        (f"insert block {i + 1}", b) for i, b in enumerate(blocks)
    ]:
        v = overlap(case, part)
        if v > 1e-3:
            problems.append(f"the case overlaps the {name} by {v:.2f} mm³")
    ledge = RIM - RIM_RELIEF
    info.append(f"glass in its pocket: {POCKET_CLEAR:g} mm round its edge, on a {ledge:g} mm ledge")
    info.append(f"LCD to the case: {gap(case, lcd):.2f} mm")
    info.append(f"post ends to the inserts: {min(gap(case, b) for b in blocks):.2f} mm")

    # The screws: the tip must stay clear of the LCD's back.
    tip = SEAT_Z - SCREW_LEN
    info.append(
        f"M3 × {SCREW_LEN:g}: {SCREW_ENGAGE:g} mm into the insert, "
        f"tip {tip - LCD_BACK_Z:.2f} mm above the LCD back"
    )
    if tip - LCD_BACK_Z < 0.5:
        problems.append("the screw tip comes within 0.5 mm of the LCD's back")

    # A USB-C plug pushed in through the window, all the way along its path.
    for name, y in PORTS.items():
        body = box(
            -OUTER_W / 2 - 40, PORT_MOUTH_X - 0.3, y - 6.5, y + 6.5, PORT_Z - 4.25, PORT_Z + 4.25
        )
        v = overlap(case, body)
        if v > 1e-3:
            problems.append(f"a 13 × 8.5 mm plug for {name} hits the case on its way in ({v:.1f})")
        chunky = box(
            -OUTER_W / 2 - 40, PORT_MOUTH_X - 0.3, y - 7.4, y + 7.4, PORT_Z - 4.75, PORT_Z + 4.75
        )
        fits = "fits" if overlap(case, chunky) < 1e-3 else "does NOT fit"
        info.append(f"{name}: a 14.8 × 9.5 mm plug {fits}")
        above = PORT_Z - 4.25 - LCD_BACK_Z
        info.append(
            f"{name}: plug path {gap(case, body):.2f} mm from the case, {above:.2f} above the LCD"
        )

    # The driver board, from Waveshare's model. The case's material inside the cavity is the
    # back plate and the posts, so only parts near those (or outside the cavity) can touch it.
    if board_step is not None:
        model = cq.importers.importStep(str(board_step))
        parts = [s.translate(cq.Vector(*PCB_OFFSET)) for s in model.solids().vals()]
        cav_x, cav_y = GLASS_W / 2 - RIM, GLASS_H / 2 - RIM

        def near(bb: cq.BoundBox, m: float) -> bool:
            if bb.zmax > BACK_IN_Z - m or bb.xmin < -cav_x + m or bb.xmax > cav_x - m:
                return True
            if bb.ymin < -cav_y + m or bb.ymax > cav_y - m:
                return True
            r = POST_D / 2 + m
            return any(
                bb.xmin < x + r
                and bb.xmax > x - r
                and bb.ymin < y + r
                and bb.ymax > y - r
                and bb.zmax > POST_END_Z - m
                for x, y in INSERTS
            )

        close = [p for p in parts if near(p.BoundingBox(), 6.0)]
        hits = [v for v in (overlap(case, p) for p in close) if v > 1e-3]
        if hits:
            problems.append(f"the case overlaps {len(hits)} parts of the driver board")
        worst = min((gap(case, p) for p in close), default=float("inf"))
        info.append(f"driver board: {len(parts)} parts, the closest {worst:.2f} mm from the case")
        top = max(p.BoundingBox().zmax for p in parts)
        info.append(
            f"driver board: tallest part at z = {top:.2f}, inside of the back at {BACK_IN_Z:.2f}"
        )

    # The stand holds the case.
    g = stand_geometry()
    seated = for_stand(case, g)
    v = overlap(stand_part, seated)
    if v > 1e-3:
        problems.append(f"the case doesn't fit the stand's slot ({v:.1f} mm³)")
    info.append(f"case in the stand: {gap(stand_part, seated):.2f} mm clearance")
    vents_from_bottom = VENT_CENTER[1] - VENT_L / 2 + OUTER_H / 2
    if vents_from_bottom < STAND_BACK_H + 2:
        problems.append("the stand's back support covers the vents")
    info.append(f"vents: {vents_from_bottom - STAND_BACK_H:.1f} mm above the stand's back support")
    # Tipping: the centre of mass must sit between the toe and the heel.
    centre_y = g["centre"][0]
    info.append(
        f"stand: centre of mass {centre_y - g['toe'][0]:.0f} mm behind the toe, "
        f"{g['heel'][0] - centre_y:.0f} mm in front of the heel"
    )

    print("\n".join("  " + s for s in info))
    return problems


# ── main ───────────────────────────────────────────────────────────────────────────────


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    here = Path(__file__).resolve().parent
    ap.add_argument("--out", type=Path, default=here, help="output folder")
    ap.add_argument(
        "--check",
        type=Path,
        metavar="BOARD_STP",
        nargs="?",
        const=False,
        default=None,
        help="check clearances (optionally against Waveshare's board model)",
    )
    args = ap.parse_args()

    print("building the back case…")
    case = back_case()
    print("building the stand…")
    stand_part = stand()
    test = fit_test(case)

    if args.check is not None:
        print("checking clearances…")
        problems = check(case, stand_part, args.check or None)
        if problems:
            print("PROBLEMS:\n" + "\n".join("  " + p for p in problems))
            return 1
        print("all clear")

    stl, step = args.out / "stl", args.out / "step"
    stl.mkdir(parents=True, exist_ok=True)
    step.mkdir(parents=True, exist_ok=True)
    parts = {"back-case": for_printing(case), "stand": stand_part, "fit-test": for_printing(test)}
    for name, part in parts.items():
        cq.exporters.export(part, str(stl / f"{name}.stl"), tolerance=0.02, angularTolerance=0.15)
        cq.exporters.export(part, str(step / f"{name}.step"))
        bb = part.BoundingBox()
        size = f"{bb.xlen:.1f} × {bb.ylen:.1f} × {bb.zlen:.1f} mm"
        print(f"{name}: {size}, {part.Volume() / 1000:.0f} cm³")
    return 0


if __name__ == "__main__":
    sys.exit(main())
