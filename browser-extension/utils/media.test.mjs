import test from 'node:test';
import assert from 'node:assert/strict';
import {
  MIN_VIDEO_HEIGHT,
  MIN_VIDEO_WIDTH,
  classifyMediaUrl,
  hasUsableBox,
  pickPrimaryVideo,
} from './media.ts';

const box = (width, height, currentSrc = '') => ({ clientWidth: width, clientHeight: height, currentSrc });

test('classifyMediaUrl reports detecting when no source is chosen yet', () => {
  assert.equal(classifyMediaUrl(''), 'detecting');
  assert.equal(classifyMediaUrl('   '), 'detecting');
});

test('classifyMediaUrl accepts absolute and relative http(s) sources', () => {
  assert.equal(classifyMediaUrl('https://cdn.example.com/clip.mp4'), 'ready');
  assert.equal(classifyMediaUrl('http://cdn.example.com/clip.mp4'), 'ready');
  assert.equal(classifyMediaUrl('/media/clip.mp4', 'https://example.com/watch'), 'ready');
});

test('classifyMediaUrl rejects non-fetchable sources', () => {
  assert.equal(classifyMediaUrl('blob:https://example.com/1234'), 'unavailable');
  assert.equal(classifyMediaUrl('data:video/mp4;base64,AAAA'), 'unavailable');
  assert.equal(classifyMediaUrl('mediasource:blob:https://example.com/x'), 'unavailable');
});

test('classifyMediaUrl treats an unparseable source as unavailable', () => {
  assert.equal(classifyMediaUrl('http://'), 'unavailable');
  assert.equal(classifyMediaUrl('::::'), 'unavailable');
});

test('hasUsableBox filters out thumbnails and zero-size players', () => {
  assert.equal(hasUsableBox(box(0, 0)), false);
  assert.equal(hasUsableBox(box(MIN_VIDEO_WIDTH - 1, MIN_VIDEO_HEIGHT)), false);
  assert.equal(hasUsableBox(box(MIN_VIDEO_WIDTH, MIN_VIDEO_HEIGHT - 1)), false);
  assert.equal(hasUsableBox(box(MIN_VIDEO_WIDTH, MIN_VIDEO_HEIGHT)), true);
});

test('pickPrimaryVideo returns the largest qualifying player', () => {
  const small = box(MIN_VIDEO_WIDTH, MIN_VIDEO_HEIGHT, 'https://cdn.example.com/small.mp4');
  const large = box(1280, 720, 'https://cdn.example.com/large.mp4');
  const thumbnail = box(160, 90, 'https://cdn.example.com/thumb.mp4');
  assert.equal(pickPrimaryVideo([small, thumbnail, large]), large);
});

test('pickPrimaryVideo returns null when nothing qualifies', () => {
  assert.equal(pickPrimaryVideo([]), null);
  assert.equal(pickPrimaryVideo([box(0, 0), box(120, 68)]), null);
});

test('pickPrimaryVideo keeps the first player on equal areas', () => {
  const first = box(640, 360, 'https://cdn.example.com/first.mp4');
  const second = box(640, 360, 'https://cdn.example.com/second.mp4');
  assert.equal(pickPrimaryVideo([first, second]), first);
});
