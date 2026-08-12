import assert from 'node:assert/strict'
import test from 'node:test'
import { deliveryPlan } from '../src/delivery.ts'

test('aggregate delivery names elementary-stream work rather than pipeline mode', () => {
  assert.equal(deliveryPlan('direct').chip, 'DIRECT')
  assert.equal(deliveryPlan('copy').chip, 'REMUX')
  assert.equal(deliveryPlan('audio_encode').chip, 'TRANSCODE')
  assert.equal(deliveryPlan('video_encode').chip, 'TRANSCODE')
})
