// SPDX-License-Identifier: Apache 2
pragma solidity ^0.8.0;

import "@pythnetwork/pyth-sdk-solidity/MockPyth.sol";

contract PythMock is MockPyth {
	constructor() MockPyth(300, 0) {}
}
