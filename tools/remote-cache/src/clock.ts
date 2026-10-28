// The gateway's clock for ages (refresh, expiry, key windows). Tests move it with `offsetMs`;
// R2's own upload dates are never moved, so a test ages an object by moving the clock past it.

export const clock = {
  offsetMs: 0,
  nowMs(): number {
    return Date.now() + this.offsetMs;
  },
  nowSeconds(): number {
    return Math.floor(this.nowMs() / 1000);
  },
};

export const DAY = 86_400;
