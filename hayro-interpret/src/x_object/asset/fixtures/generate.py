"""Generate synthetic JPEG row integration fixtures with libjpeg-turbo cjpeg."""
import pathlib
import subprocess

root = pathlib.Path(__file__).parent
for width, height, restart in [(7, 9, 0), (33, 41, 3), (257, 4097, 7), (32769, 529, 5)]:
    pixels = bytes((x * 17 + y * 13 + x * y % 37) & 255 for y in range(height) for x in range(width)) if width < 1000 else bytes((y // 8 * 17) & 255 for y in range(height) for _ in range(width))
    flags = ["-quality", "91", "-grayscale"]
    if restart:
        flags += ["-restart", f"{restart}B"]
    pgm = f"P5\n{width} {height}\n255\n".encode() + pixels
    result = subprocess.run(["cjpeg", *flags], input=pgm, capture_output=True, check=True)
    (root / f"{width}x{height}.jpg").write_bytes(result.stdout)
