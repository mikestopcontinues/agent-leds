// How a value sent to the SidePulse Pro looks, as closely as a monitor can show it.
//
// The Pro's lights are WS2816 RGB LEDs behind a frosted diffuser. Three facts
// make a hex value look different on them than on a screen:
//
// 1. Light output is linear in the value (constant-current PWM, and the
//    firmware applies no gamma), whereas a screen reads the same value through
//    the sRGB curve. #808080 is half the light of white on the device but only
//    about a fifth on a screen.
// 2. The three dies are nearly single-wavelength (about 627, 527 and 467 nm),
//    so their colors lie outside what sRGB, and partly what Display P3, can show.
// 3. Firmware 1.1 balances the channel gains so that equal values make white.
//
// The numbers below come from the WS2816 datasheet, not from measuring this
// device, so the model is a starting point to tune against the hardware.

export type RGB = [number, number, number];
type Vec = [number, number, number];
type Mat = [Vec, Vec, Vec];
type XY = [number, number];

export type Model = {
  /** Exponent applied to value/255 before it becomes light. 1 is the linear response the hardware has. */
  response: number;
  /** Multiplies the light, for matching how bright the LEDs look where you sit. */
  exposure: number;
};

/** The response the hardware has by its datasheet. */
export const LINEAR_MODEL: Model = { response: 1, exposure: 1 };

/** What matched the device by eye: dim values look brighter still than their share of the light. */
export const DEFAULT_MODEL: Model = { response: 0.6, exposure: 1 };

const D65: XY = [0.3127, 0.329];

/** Chromaticities of the dies, estimated from the datasheet's dominant wavelengths. */
const LED_PRIMARIES: [XY, XY, XY] = [
  [0.703, 0.297],
  [0.17, 0.73],
  [0.13, 0.06],
];
const SRGB_PRIMARIES: [XY, XY, XY] = [
  [0.64, 0.33],
  [0.3, 0.6],
  [0.15, 0.06],
];
const P3_PRIMARIES: [XY, XY, XY] = [
  [0.68, 0.32],
  [0.265, 0.69],
  [0.15, 0.06],
];

function mul(m: Mat, v: Vec): Vec {
  return m.map((row) => row[0] * v[0] + row[1] * v[1] + row[2] * v[2]) as Vec;
}

function invert(m: Mat): Mat {
  const [[a, b, c], [d, e, f], [g, h, i]] = m;
  const det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
  return [
    [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det],
    [(f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det],
    [(d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det],
  ];
}

function xyToXyz([x, y]: XY): Vec {
  return [x / y, 1, (1 - x - y) / y];
}

/** The matrix taking linear RGB in a space with these primaries to XYZ, with white at Y = 1. */
function rgbToXyz(primaries: [XY, XY, XY], white: XY): Mat {
  const [r, g, b] = primaries.map(xyToXyz) as [Vec, Vec, Vec];
  const unscaled: Mat = [
    [r[0], g[0], b[0]],
    [r[1], g[1], b[1]],
    [r[2], g[2], b[2]],
  ];
  const scale = mul(invert(unscaled), xyToXyz(white));
  return unscaled.map((row) => row.map((value, column) => value * scale[column]!)) as Mat;
}

const LED_TO_XYZ = rgbToXyz(LED_PRIMARIES, D65);
const XYZ_TO_LED = invert(LED_TO_XYZ);
const SRGB_TO_XYZ = rgbToXyz(SRGB_PRIMARIES, D65);
const XYZ_TO_DISPLAY = { srgb: invert(SRGB_TO_XYZ), p3: invert(rgbToXyz(P3_PRIMARIES, D65)) };

export type Gamut = keyof typeof XYZ_TO_DISPLAY;

function encode(linear: number): number {
  const value = Math.min(1, Math.max(0, linear));
  return value <= 0.0031308 ? 12.92 * value : 1.055 * value ** (1 / 2.4) - 0.055;
}

function decode(encoded: number): number {
  return encoded <= 0.04045 ? encoded / 12.92 : ((encoded + 0.055) / 1.055) ** 2.4;
}

/** The light the device emits for a value, as CIE XYZ with full white at Y = 1. */
function emitted(rgb: RGB, model: Model): Vec {
  const light = rgb.map((value) => (value / 255) ** model.response * model.exposure) as Vec;
  return mul(LED_TO_XYZ, light);
}

/**
 * How a device value looks, as encoded 0-1 components in the given display gamut.
 *
 * A color more saturated than the display can show gets just enough white
 * added to fit, which keeps its dominant wavelength. A color brighter than the
 * display's maximum is dimmed rather than washed out, because a saturated LED
 * looks saturated however bright it is.
 */
export function appearance(rgb: RGB, model: Model, gamut: Gamut): Vec {
  const linear = mul(XYZ_TO_DISPLAY[gamut], emitted(rgb, model));
  const white = -Math.min(0, ...linear);
  const peak = Math.max(1, ...linear.map((value) => value + white));
  return linear.map((value) => encode((value + white) / peak)) as Vec;
}

function byte(component: number): number {
  return Math.round(Math.min(1, Math.max(0, component)) * 255);
}

export function toHex(rgb: RGB): string {
  return "#" + rgb.map((value) => value.toString(16).padStart(2, "0")).join("").toUpperCase();
}

export function fromHex(hex: string): RGB | null {
  const match = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!match) return null;
  const digits = match[1]!;
  return [0, 2, 4].map((at) => parseInt(digits.slice(at, at + 2), 16)) as RGB;
}

/** How a device value looks, as the nearest sRGB hex. */
export function appearanceHex(rgb: RGB, model: Model): string {
  return toHex(appearance(rgb, model, "srgb").map(byte) as RGB);
}

/** A CSS color for how a device value looks, or for the raw hex when `trueColor` is off. */
export function css(rgb: RGB, trueColor: boolean, model: Model, gamut: Gamut): string {
  if (!trueColor) return toHex(rgb);
  const [r, g, b] = appearance(rgb, model, gamut).map((value) => value.toFixed(4));
  return gamut === "p3" ? `color(display-p3 ${r} ${g} ${b})` : `rgb(${byte(+r!)} ${byte(+g!)} ${byte(+b!)})`;
}

/** The device value whose light is closest to a color as it looks on an sRGB screen. */
export function deviceColorFor(srgbHex: string, model: Model): RGB | null {
  const target = fromHex(srgbHex);
  if (!target) return null;
  const light = mul(XYZ_TO_LED, mul(SRGB_TO_XYZ, target.map((value) => decode(value / 255)) as Vec));
  return light.map((value) => byte(Math.min(1, Math.max(0, value / model.exposure)) ** (1 / model.response))) as RGB;
}
