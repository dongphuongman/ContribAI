const { test } = require("node:test");
const assert = require("node:assert/strict");
const { sum, describe } = require("../index.js");

test("sum adds two numbers", () => {
  assert.equal(sum(2, 3), 5);
});

test("describe returns label", () => {
  assert.equal(describe(), "adder");
});
