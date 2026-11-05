#!/usr/bin/env python3
"""A `script` capture as text (lane tui): render.py CAPTURE.tty [EVERY]

Plays the capture on a screen that knows the four things the live region writes (CR, LF,
cursor up, erase below; colours are dropped), and prints every EVERYth frame the region drew
(each up to the cursor going back over it), then the screen as it was left.
"""

import re
import sys

data = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
every = int(sys.argv[2]) if len(sys.argv) > 2 else 10
data = data.split("\n", 1)[1].rsplit("\nScript done", 1)[0]
data = re.sub(r"\x1b\[[0-9;]*m", "", data)

lines, row, col = [""], 0, 0
frames = []
for token in re.findall(r"\x1b\[(\d*)([AJ])|(\r)|(\n)|([^\x1b\r\n]+)", data):
    up, code, cr, lf, text = token
    if code == "A":
        frames.append(lines[row - int(up or 1) : row])
        row -= int(up or 1)
    elif code == "J":
        lines[row] = lines[row][:col]
        del lines[row + 1 :]
    elif cr:
        col = 0
    elif lf:
        row += 1
        col = 0
        if row == len(lines):
            lines.append("")
    else:
        line = lines[row]
        lines[row] = line[:col] + text + line[col + len(text) :]
        col += len(text)

for n, frame in enumerate(frames):
    if n % every == 0:
        print(f"--- frame {n}")
        print("\n".join(frame))
print(f"--- left on the screen ({len(frames)} frames drawn)")
print("\n".join(lines).rstrip("\n"))
