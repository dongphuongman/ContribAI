import assert from "node:assert/strict";
import { greet } from "../src/greeting.js";

assert.equal(greet("maintainer"), "Hello, maintainer!");
console.log("fixture test passed");
