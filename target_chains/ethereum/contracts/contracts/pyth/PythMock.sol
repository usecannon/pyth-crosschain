// SPDX-License-Identifier: Apache 2
pragma solidity ^0.8.0;

import "@pythnetwork/pyth-sdk-solidity/MockPyth.sol";

contract PythMock is MockPyth {
	constructor(uint _validTimePeriod, uint _singleUpdateFeeInWei) MockPyth(_validTimePeriod, _singleUpdateFeeInWei) {}
}
