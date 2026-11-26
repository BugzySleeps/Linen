import math
def shape(n, a, rot=0.0, N=120):
    R = 50 / (1 + a); pts = []
    for i in range(N):
        t = i / N * 2 * math.pi
        r = R * (1 + a * math.cos(n * (t + rot)))
        pts.append(f"{50 + r*math.cos(t):.1f} {50 + r*math.sin(t):.1f}")
    return "M" + "L".join(pts) + "Z"
SH = {"cookie9": shape(9, .08), "cookie12": shape(12, .055), "flower": shape(6, .14), "sunny": shape(8, .05), "clover": shape(4, .17, math.pi/4), "burst": shape(10, .09)}
TRI = "M-26.06,-43.00 Q-26.06,-65.00 -7.01,-54.00 L57.07,-17.00 Q76.13,-6.00 57.07,5.00 L-7.01,42.00 Q-26.06,53.00 -26.06,31.00 Z"
TAIL = "M -31,53 C -40,78 -70,74 -66,52 C -63,36 -40,44 -52,70"
def logo(cls="", w=16, t=10):
    return (f'<svg class="mark {cls}" viewBox="-100 -94 200 200" aria-hidden="true">'
            f'<path class="tri" d="{TRI}" pathLength="1" fill="none" stroke="currentColor" stroke-width="{w}" stroke-linejoin="round"/>'
            f'<path class="tail" d="{TAIL}" pathLength="1" fill="none" stroke="currentColor" stroke-width="{t}" stroke-linecap="round"/></svg>')
def icon(name):  # Material-style stroke icons, inline
    P = {
     "bolt": '<path d="M13 2 4 14h7l-1 8 9-12h-7z"/>',
     "skip": '<path d="M5 5l9 7-9 7z"/><path d="M18 5v14"/>',
     "spark": '<path d="M12 3l2 5 5 2-5 2-2 5-2-5-5-2 5-2z"/><path d="M19 15l.8 2 2 .8-2 .8-.8 2-.8-2-2-.8 2-.8z"/>',
     "leaf": '<path d="M5 19c0-8 5-14 15-14 0 10-6 15-14 15"/><path d="M5 19l7-7"/>',
     "head": '<path d="M4 15v-3a8 8 0 0 1 16 0v3"/><rect x="3" y="14" width="4" height="6" rx="1.6"/><rect x="17" y="14" width="4" height="6" rx="1.6"/>',
     "down": '<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>',
     "chap": '<path d="M3 12h4M9 12h6M17 12h4"/><circle cx="8" cy="12" r="1.6"/><circle cx="16" cy="12" r="1.6"/>',
     "pip": '<rect x="3" y="5" width="18" height="14" rx="3"/><rect x="12" y="11" width="7" height="6" rx="1.5"/>',
     "keys": '<rect x="3" y="7" width="18" height="10" rx="2.5"/><path d="M10 10l4 2-4 2z"/>',
     "chat": '<path d="M5 5h14a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H10l-5 4v-4a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2z"/>',
     "list": '<path d="M4 7h11M4 12h11M4 17h7"/><path d="M17 14l4 2.5-4 2.5z"/>',
     "lock": '<rect x="5" y="11" width="14" height="9" rx="2.5"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/>',
     "chip": '<rect x="6" y="6" width="12" height="12" rx="2.5"/><path d="M9 3v3M15 3v3M9 18v3M15 18v3M3 9h3M3 15h3M18 9h3M18 15h3"/>',
     "copy": '<rect x="8" y="8" width="12" height="12" rx="2.5"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>',
     "gh": '',
    }
    return f'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{P[name]}</svg>'
def blob(shape, ic, fill="var(--chip)"):
    return (f'<span class="blob"><svg viewBox="0 0 100 100" aria-hidden="true"><path d="{SH[shape]}" fill="{fill}"/></svg>{icon(ic)}</span>')
GH = '<svg viewBox="0 0 24 24" aria-hidden="true"><path fill="currentColor" d="M12 2a10 10 0 0 0-3.16 19.49c.5.09.68-.22.68-.48v-1.7c-2.78.6-3.37-1.34-3.37-1.34-.45-1.16-1.11-1.47-1.11-1.47-.91-.62.07-.6.07-.6 1 .07 1.53 1.03 1.53 1.03.9 1.52 2.34 1.08 2.91.83.09-.65.35-1.08.63-1.33-2.22-.25-4.56-1.11-4.56-4.94 0-1.09.39-1.98 1.03-2.68-.1-.25-.45-1.27.1-2.64 0 0 .84-.27 2.75 1.02a9.6 9.6 0 0 1 5 0c1.91-1.29 2.75-1.02 2.75-1.02.55 1.37.2 2.39.1 2.64.64.7 1.03 1.59 1.03 2.68 0 3.84-2.34 4.69-4.57 4.93.36.31.68.92.68 1.85v2.75c0 .27.18.58.69.48A10 10 0 0 0 12 2z"/></svg>'

html = open(__file__.replace("build-site.py", "site-page.html")).read()
for k, v in {"{{LOGO}}": logo(), "{{LOGO_HERO}}": logo("hero-mark", 14, 9), "{{GH}}": GH,
             "{{COOKIE9}}": SH["cookie9"], "{{FLOWER}}": SH["flower"], "{{SUNNY}}": SH["sunny"], "{{CLOVER}}": SH["clover"], "{{COOKIE12}}": SH["cookie12"]}.items():
    html = html.replace(k, v)
import re
html = re.sub(r"\{\{B:(\w+):(\w+)\}\}", lambda m: blob(m.group(1), m.group(2)), html)
html = re.sub(r"\{\{I:(\w+)\}\}", lambda m: icon(m.group(1)), html)
assert "{{" not in html, re.findall(r"\{\{[^}]*\}\}", html)
open(__file__.replace("design/build-site.py", "docs/index.html"), "w").write(html)
print("ok", len(html))
