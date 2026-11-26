import math, sys
OUT = sys.argv[1]
A, ON, BG, TXT, DIM = "#F0B892", "#3B2314", "#121212", "#ECEAE6", "#A8A49E"

def shape(n, a, cx, cy, r, rot=0.0, N=180):
    R = r / (1 + a); pts = []
    for i in range(N):
        t = i / N * 2 * math.pi
        rr = R * (1 + a * math.cos(n * (t + rot)))
        pts.append(f"{cx + rr*math.cos(t):.2f},{cy + rr*math.sin(t):.2f}")
    return "M" + " L".join(pts) + "Z"

def rtri(cx, cy, s, rad):
    # play triangle (pointing right) with rounded corners
    h = s * math.sqrt(3) / 2
    P = [(cx - h/3, cy - s/2), (cx + 2*h/3, cy), (cx - h/3, cy + s/2)]
    d = ""
    for i in range(3):
        p0, p1, p2 = P[i-1], P[i], P[(i+1) % 3]
        def toward(a, b, k):
            L = math.dist(a, b); return (a[0] + (b[0]-a[0])*k/L, a[1] + (b[1]-a[1])*k/L)
        s1, s2 = toward(p1, p0, rad), toward(p1, p2, rad)
        d += (f"M{s1[0]:.2f},{s1[1]:.2f}" if i == 0 else f" L{s1[0]:.2f},{s1[1]:.2f}") + f" Q{p1[0]:.2f},{p1[1]:.2f} {s2[0]:.2f},{s2[1]:.2f}"
    return d + " Z"

def squircle(cx, cy, r, k=0.42):
    # rounded square
    return f'<rect x="{cx-r}" y="{cy-r}" width="{2*r}" height="{2*r}" rx="{r*k}"/>'

# each mark drawn around (0,0) in a 200 box
def mark_burst():
    return f'<path d="{shape(10,0.09,0,0,92)}" fill="{A}"/><text x="0" y="31" text-anchor="middle" font-family="Manrope" font-weight="800" font-size="92" fill="{ON}">L</text>'
def mark_cookie():
    return f'<path d="{shape(9,0.08,0,0,92)}" fill="{A}"/><path d="{rtri(6,0,78,12)}" fill="{ON}"/>'
def mark_weave():
    out = [f'<g fill="{A}">{squircle(0,0,86)}</g>']
    w, pos, L = 24, [-40, 0, 40], 64
    edge = 'stroke="' + A + '" stroke-width="6"'
    # all strands: verticals, then horizontals on top
    for x in pos: out.append(f'<rect x="{x-w/2}" y="{-L-w/2}" width="{w}" height="{2*L+w}" rx="{w/2}" fill="{ON}" {edge}/>')
    for y in pos: out.append(f'<rect x="{-L-w/2}" y="{y-w/2}" width="{2*L+w}" height="{w}" rx="{w/2}" fill="{ON}" {edge}/>')
    # where the vertical goes over, put it back on top (edges left and right only)
    for i, y in enumerate(pos):
        for j, x in enumerate(pos):
            if (i + j) % 2 == 1:
                h = w + 10
                out.append(f'<rect x="{x-w/2}" y="{y-h/2}" width="{w}" height="{h}" fill="{ON}"/>')
                out.append(f'<line x1="{x-w/2}" y1="{y-h/2}" x2="{x-w/2}" y2="{y+h/2}" stroke="{A}" stroke-width="6"/>')
                out.append(f'<line x1="{x+w/2}" y1="{y-h/2}" x2="{x+w/2}" y2="{y+h/2}" stroke="{A}" stroke-width="6"/>')
    return "".join(out)
def mark_thread():
    tri = rtri(8, -6, 118, 22)
    tail = "M -31,53 C -40,78 -70,74 -66,52 C -63,36 -40,44 -52,70"
    return (f'<path d="{tri}" fill="none" stroke="{A}" stroke-width="16" stroke-linejoin="round"/>'
            f'<path d="{tail}" fill="none" stroke="{A}" stroke-width="10" stroke-linecap="round"/>')
def mark_flower():
    return f'<path d="{shape(6,0.2,0,0,94,math.pi/6)}" fill="{A}"/><path d="{rtri(5,0,62,10)}" fill="{ON}"/>'
def mark_fold():
    # a play shape folded from one soft ribbon: two overlapping shades
    back = rtri(-6, 0, 150, 26)
    front = rtri(14, 0, 104, 20)
    return f'<path d="{back}" fill="{A}" opacity="0.45"/><path d="{front}" fill="{A}"/>'

marks = [
    ("A", "Burst L", "The rail's logo today: a soft burst with an L.", mark_burst),
    ("B", "Cookie play", "The player's scalloped play button, as the icon.", mark_cookie),
    ("C", "Woven", "Linen is a weave: threads over and under.", mark_weave),
    ("D", "Thread", "A play button drawn as one thread, loose end and all.", mark_thread),
    ("E", "Bloom", "A six-petal flower with play in the middle.", mark_flower),
    ("F", "Folded", "Two layers of soft cloth folded into play.", mark_fold),
]

W, H = 1800, 1230
s = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">',
     f'<rect width="{W}" height="{H}" fill="{BG}"/>',
     f'<text x="80" y="110" font-family="Manrope" font-weight="800" font-size="56" fill="{TXT}">Linen logos</text>',
     f'<text x="80" y="160" font-family="Manrope" font-weight="500" font-size="24" fill="{DIM}">Pick one (or mix): each shown big, on a light background, and at taskbar sizes.</text>']
cw, ch = 540, 470
for k, (letter, name, desc, fn) in enumerate(marks):
    col, row = k % 3, k // 3
    x0, y0 = 80 + col * (cw + 40), 210 + row * (ch + 30)
    s.append(f'<rect x="{x0}" y="{y0}" width="{cw}" height="{ch}" rx="36" fill="#ffffff0c" stroke="#ffffff14"/>')
    # big, on dark
    s.append(f'<g transform="translate({x0+150},{y0+170})">{fn()}</g>')
    # on light tile
    s.append(f'<rect x="{x0+320}" y="{y0+60}" width="180" height="180" rx="40" fill="#F4F1EC"/>')
    s.append(f'<g transform="translate({x0+410},{y0+150}) scale(0.62)">{fn()}</g>')
    # small sizes
    for i, sc in enumerate([0.24, 0.16, 0.11]):
        cx = x0 + 340 + [0, 62, 108][i]
        s.append(f'<g transform="translate({cx},{y0+300}) scale({sc})">{fn()}</g>')
    s.append(f'<text x="{x0+40}" y="{y0+372}" font-family="Manrope" font-weight="800" font-size="34" fill="{TXT}"><tspan fill="{A}">{letter}</tspan>  {name}</text>')
    s.append(f'<text x="{x0+40}" y="{y0+414}" font-family="Manrope" font-weight="500" font-size="21" fill="{DIM}">{desc}</text>')
    # wordmark preview
    s.append(f'<g transform="translate({x0+60},{y0+300}) scale(0.2)">{fn()}</g><text x="{x0+90}" y="{y0+310}" font-family="Manrope" font-weight="800" font-size="30" fill="{TXT}">linen</text>')
s.append("</svg>")
open(OUT, "w").write("\n".join(s))
