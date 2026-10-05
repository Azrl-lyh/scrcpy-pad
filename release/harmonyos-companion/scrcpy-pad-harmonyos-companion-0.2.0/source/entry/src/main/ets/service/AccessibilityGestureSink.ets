import { AccessibilityExtensionContext, GesturePath, GesturePoint } from '@kit.AccessibilityKit';
import { hilog } from '@kit.PerformanceAnalysisKit';
import { GestureSink } from '../engine/GestureSink';
import { Point } from '../model/Keymap';

const DOMAIN = 0x5343;
const HOLD_WINDOW_MS: number = 180;
const HOLD_RETRY_MS: number = 12;

class HoldTarget {
  x: number = 0;
  y: number = 0;
  generation: number = 0;
  running: boolean = false;
}

export class AccessibilityGestureSink implements GestureSink {
  private context: AccessibilityExtensionContext;
  private holds: Map<string, HoldTarget> = new Map<string, HoldTarget>();
  private generation: number = 0;
  private chain: Promise<void> = Promise.resolve();
  private queueGeneration: number = 0;

  constructor(context: AccessibilityExtensionContext) {
    this.context = context;
  }

  tap(x: number, y: number, durationMs: number): void {
    this.enqueue([new Point(x, y), new Point(x, y)], Math.max(20, durationMs));
  }

  swipe(points: Point[], durationMs: number): void {
    this.enqueue(points, Math.max(20, durationMs));
  }

  beginHold(token: string, x: number, y: number): void {
    this.generation += 1;
    const target = new HoldTarget();
    target.x = x;
    target.y = y;
    target.generation = this.generation;
    this.holds.set(token, target);
    this.runHold(token, target);
  }

  moveHold(token: string, x: number, y: number): void {
    const target = this.holds.get(token);
    if (target !== undefined) {
      target.x = x;
      target.y = y;
    }
  }

  endHold(token: string): void {
    const target = this.holds.get(token);
    if (target !== undefined) {
      target.generation = -1;
      this.holds.delete(token);
    }
  }

  cancelAll(): void {
    this.queueGeneration += 1;
    const keys = Array.from(this.holds.keys());
    for (let index = 0; index < keys.length; index += 1) {
      this.endHold(keys[index]);
    }
  }

  private async runHold(token: string, target: HoldTarget): Promise<void> {
    if (target.running) {
      return;
    }
    target.running = true;
    while (true) {
      const current = this.holds.get(token);
      if (current === undefined || current.generation !== target.generation) {
        target.running = false;
        return;
      }
      this.enqueue([new Point(target.x, target.y), new Point(target.x, target.y)], HOLD_WINDOW_MS);
      await this.delay(HOLD_WINDOW_MS + HOLD_RETRY_MS);
    }
  }

  private enqueue(points: Point[], durationMs: number): void {
    if (points.length === 0) {
      return;
    }
    const captured = points.slice();
    const generation = this.queueGeneration;
    this.chain = this.chain.then(async () => {
      if (generation !== this.queueGeneration) {
        return;
      }
      await this.injectOnce(captured, durationMs);
    }).catch((error: Error) => {
      hilog.warn(DOMAIN, 'GestureSink', 'injectGesture failed: %{public}s', JSON.stringify(error));
    });
  }

  private async injectOnce(points: Point[], durationMs: number): Promise<void> {
    const path = new GesturePath(durationMs);
    for (let index = 0; index < points.length; index += 1) {
      path.points.push(new GesturePoint(points[index].x, points[index].y));
    }
    try {
      await this.context.injectGesture(path);
    } catch (error) {
      hilog.warn(DOMAIN, 'GestureSink', 'injectGesture failed: %{public}s', JSON.stringify(error));
    }
  }

  private delay(milliseconds: number): Promise<void> {
    return new Promise<void>((resolve: () => void) => {
      setTimeout(resolve, milliseconds);
    });
  }
}
