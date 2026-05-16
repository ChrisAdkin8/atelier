import { test } from 'node:test';
import assert from 'node:assert';
import { divisibleBy } from '../utils.ts';

test('6 is divisible by 2', () => {
  assert.strictEqual(divisibleBy(6, 2), true);
});

test('7 is not divisible by 2', () => {
  assert.strictEqual(divisibleBy(7, 2), false);
});

test('0 is divisible by 5', () => {
  assert.strictEqual(divisibleBy(0, 5), true);
});

test('divisibleBy(5, 0) throws', () => {
  assert.throws(() => divisibleBy(5, 0));
});
