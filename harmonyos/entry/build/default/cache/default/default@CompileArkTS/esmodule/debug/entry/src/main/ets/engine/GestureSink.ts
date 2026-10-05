import type { Point } from '../model/Keymap';
export interface GestureSink {
    tap(x: number, y: number, durationMs: number): void;
    swipe(points: Point[], durationMs: number): void;
    beginHold(token: string, x: number, y: number): void;
    moveHold(token: string, x: number, y: number): void;
    endHold(token: string): void;
    cancelAll(): void;
}
export class NoopGestureSink implements GestureSink {
    tap(x: number, y: number, durationMs: number): void {
    }
    swipe(points: Point[], durationMs: number): void {
    }
    beginHold(token: string, x: number, y: number): void {
    }
    moveHold(token: string, x: number, y: number): void {
    }
    endHold(token: string): void {
    }
    cancelAll(): void {
    }
}
