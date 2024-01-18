#!/bin/bash

set -x
set -e

for f in networks/*.json; do
  chainId=${f#"networks/"}
  chainId=${chainId%".json"}
	addr=$(jq -r '.[] | select(.contractName=="PythUpgradable") | .address' < $f)

	echo "setup network $chainId ($addr)"

	cannon alter pyth:1.4.1 --chain-id $chainId set-url $1
	cannon alter pyth:1.4.1 --chain-id $chainId set-contract-address Pyth $addr
done

echo 'done'
