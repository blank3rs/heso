// heso is a CLI, not a library. This file exists so `require("@ixla/heso")`
// doesn't error in case someone imports the package by mistake. The real
// entry point is `bin/heso.js`, exposed as the `heso` command.
module.exports = {};
