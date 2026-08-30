export const COLLAPSED_ISLAND_WIDTH = 56;
export const DEFAULT_SIDEBAR_WIDTH = 260;
export const DEFAULT_RIGHT_PANEL_WIDTH = 340;
export const MIN_ISLAND_WIDTH = 190;
export const MAX_ISLAND_WIDTH = 520;
export const COLLAPSE_THRESHOLD = 80;
export const MIN_MAIN_WIDTH = 360;
export const SHELL_HORIZONTAL_SPACE = 48;

export function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value));
}

export function fitExpandedIslands(
  containerWidth: number,
  left: number,
  right: number,
  leftCollapsed: boolean,
  rightCollapsed: boolean,
): [number, number] {
  const total = containerWidth - SHELL_HORIZONTAL_SPACE - MIN_MAIN_WIDTH;
  const dragMinimum = COLLAPSE_THRESHOLD + 1;
  let nextLeft = clamp(left, MIN_ISLAND_WIDTH, MAX_ISLAND_WIDTH);
  let nextRight = clamp(right, MIN_ISLAND_WIDTH, MAX_ISLAND_WIDTH);

  if (leftCollapsed && rightCollapsed) return [nextLeft, nextRight];
  if (leftCollapsed) {
    const available = Math.max(dragMinimum, total - COLLAPSED_ISLAND_WIDTH);
    return [nextLeft, Math.min(nextRight, available)];
  }
  if (rightCollapsed) {
    const available = Math.max(dragMinimum, total - COLLAPSED_ISLAND_WIDTH);
    return [Math.min(nextLeft, available), nextRight];
  }

  const budget = Math.max(dragMinimum * 2, total);
  const floor = Math.min(MIN_ISLAND_WIDTH, budget / 2);
  const overflow = nextLeft + nextRight - budget;
  if (overflow <= 0) return [nextLeft, nextRight];
  const leftRoom = nextLeft - floor;
  const rightRoom = nextRight - floor;
  const room = leftRoom + rightRoom;
  if (room <= 0) return [floor, floor];
  nextLeft -= overflow * (leftRoom / room);
  nextRight -= overflow * (rightRoom / room);
  return [Math.max(floor, nextLeft), Math.max(floor, nextRight)];
}
