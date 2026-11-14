import { expect, test } from "bun:test";
import { LINEAR_MODEL, appearance, appearanceHex, deviceColorFor, fromHex, type RGB } from "./color";

const look = (hex: string, model = LINEAR_MODEL) => appearanceHex(fromHex(hex)!, model);

function distance(a: string, b: string): number {
  const [x, y] = [fromHex(a)!, fromHex(b)!];
  return Math.max(...x.map((value, channel) => Math.abs(value - y[channel]!)));
}

test("equal values are neutral, and half the value is far brighter than on a screen", () => {
  expect(look("#FFFFFF")).toBe("#FFFFFF");
  expect(look("#000000")).toBe("#000000");
  // Half the light of white is sRGB 188, not 128.
  expect(look("#808080")).toBe("#BCBCBC");
});

test("a single die is shown at the edge of the display's gamut", () => {
  for (const die of [[255, 0, 0], [0, 255, 0], [0, 0, 255]] as RGB[]) {
    for (const gamut of ["srgb", "p3"] as const) {
      const shown = appearance(die, LINEAR_MODEL, gamut);
      expect(Math.min(...shown)).toBe(0);
      expect(Math.max(...shown)).toBeCloseTo(1, 5);
    }
  }
});

test("a mixed color leans further toward its second die than the same hex on a screen", () => {
  const green = (hex: string) => fromHex(hex)![1];
  expect(green(look("#FF8000"))).toBeGreaterThan(green("#FF8000"));
});

test("the response curve is what separates the device from a screen", () => {
  // With a screen-like response the same grey comes back.
  expect(distance(look("#808080", { response: 2.2, exposure: 1 }), "#808080")).toBeLessThanOrEqual(2);
  expect(fromHex(look("#404040", { response: 1, exposure: 2 }))![0]).toBeGreaterThan(fromHex(look("#404040"))![0]);
});

test("picking a look returns the value that produces it", () => {
  for (const wanted of ["#FF8000", "#00C8A0", "#FF4080", "#808080", "#3050C0"]) {
    const sent = deviceColorFor(wanted, LINEAR_MODEL)!;
    expect(distance(appearanceHex(sent, LINEAR_MODEL), wanted)).toBeLessThanOrEqual(2);
  }
});
