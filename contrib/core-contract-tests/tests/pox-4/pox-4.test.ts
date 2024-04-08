import crypto from "node:crypto";

import { assert, beforeAll, beforeEach, describe, expect, it } from "vitest";

import {
  Cl,
  ClarityType,
  getAddressFromPrivateKey,
  TransactionVersion,
  createStacksPrivateKey,
  isClarityType,
  StacksPrivateKey,
  UIntCV,
  cvToString,
} from "@stacks/transactions";
import { StacksDevnet } from "@stacks/network";
import {
  getPublicKeyFromPrivate,
  publicKeyToBtcAddress,
} from "@stacks/encryption";
import {
  Pox4SignatureTopic,
  StackingClient,
  poxAddressToTuple,
} from "@stacks/stacking";
import { tuple } from "fast-check";

const accounts = simnet.getAccounts();
const address1 = accounts.get("wallet_1")!;
const address2 = accounts.get("wallet_2")!;

const POX_DEPLOYER = "ST000000000000000000002AMW42H";
const POX_CONTRACT = `${POX_DEPLOYER}.pox-4`;

// Error codes from the contract
const ERR_STACKING_UNREACHABLE = 255;
const ERR_STACKING_CORRUPTED_STATE = 254;
const ERR_STACKING_INSUFFICIENT_FUNDS = 1;
const ERR_STACKING_INVALID_LOCK_PERIOD = 2;
const ERR_STACKING_ALREADY_STACKED = 3;
const ERR_STACKING_NO_SUCH_PRINCIPAL = 4;
const ERR_STACKING_EXPIRED = 5;
const ERR_STACKING_STX_LOCKED = 6;
const ERR_STACKING_PERMISSION_DENIED = 9;
const ERR_STACKING_THRESHOLD_NOT_MET = 11;
const ERR_STACKING_POX_ADDRESS_IN_USE = 12;
const ERR_STACKING_INVALID_POX_ADDRESS = 13;
const ERR_STACKING_INVALID_AMOUNT = 18;
const ERR_NOT_ALLOWED = 19;
const ERR_STACKING_ALREADY_DELEGATED = 20;
const ERR_DELEGATION_EXPIRES_DURING_LOCK = 21;
const ERR_DELEGATION_TOO_MUCH_LOCKED = 22;
const ERR_DELEGATION_POX_ADDR_REQUIRED = 23;
const ERR_INVALID_START_BURN_HEIGHT = 24;
const ERR_NOT_CURRENT_STACKER = 25;
const ERR_STACK_EXTEND_NOT_LOCKED = 26;
const ERR_STACK_INCREASE_NOT_LOCKED = 27;
const ERR_DELEGATION_NO_REWARD_SLOT = 28;
const ERR_DELEGATION_WRONG_REWARD_SLOT = 29;
const ERR_STACKING_IS_DELEGATED = 30;
const ERR_STACKING_NOT_DELEGATED = 31;
const ERR_INVALID_SIGNER_KEY = 32;
const ERR_REUSED_SIGNER_KEY = 33;
const ERR_DELEGATION_ALREADY_REVOKED = 34;
const ERR_INVALID_SIGNATURE_PUBKEY = 35;
const ERR_INVALID_SIGNATURE_RECOVER = 36;
const ERR_INVALID_REWARD_CYCLE = 37;
const ERR_SIGNER_AUTH_AMOUNT_TOO_HIGH = 38;
const ERR_SIGNER_AUTH_USED = 39;
const ERR_INVALID_INCREASE = 40;

// Keys to use for stacking
// wallet_1, wallet_2, wallet_3 private keys
const stackingKeys = [
  "7287ba251d44a4d3fd9276c88ce34c5c52a038955511cccaf77e61068649c17801",
  "530d9f61984c888536871c6573073bdfc0058896dc1adfe9a6a10dfacadc209101",
  "d655b2523bcd65e34889725c73064feb17ceb796831c0e111ba1a552b0f31b3901",
];

type StackerInfo = {
  authId: number;
  privKey: string;
  pubKey: string;
  stxAddress: string;
  btcAddr: string;
  signerPrivKey: StacksPrivateKey;
  signerPubKey: string;
  client: StackingClient;
};

const stackers = Object.freeze(
  stackingKeys.map((privKey, i) => {
    const network = new StacksDevnet();

    const pubKey = getPublicKeyFromPrivate(privKey);
    const stxAddress = getAddressFromPrivateKey(
      privKey,
      TransactionVersion.Testnet
    );
    const signerPrivKey = createStacksPrivateKey(privKey);
    const signerPubKey = getPublicKeyFromPrivate(signerPrivKey.data);

    const info: StackerInfo = {
      authId: i,
      privKey,
      pubKey,
      stxAddress,
      btcAddr: publicKeyToBtcAddress(pubKey),
      signerPrivKey: signerPrivKey,
      signerPubKey: signerPubKey,
      client: new StackingClient(stxAddress, network),
    };
    return info;
  })
);

const getPoxInfo = () => {
  const poxInfo = simnet.callReadOnlyFn(
    POX_CONTRACT,
    "get-pox-info",
    [],
    address1
  );
  // @ts-ignore
  const data = poxInfo.result.value.data;
  const typedPoxInfo = {
    firstBurnchainBlockHeight: data["first-burnchain-block-height"]
      .value as bigint,
    minAmountUstx: data["min-amount-ustx"].value as bigint,
    prepareCycleLength: data["prepare-cycle-length"].value as bigint,
    rewardCycleId: data["reward-cycle-id"].value as bigint,
    rewardCycleLength: data["reward-cycle-length"].value as bigint,
    totalLiquidSupplyUstx: data["total-liquid-supply-ustx"].value as bigint,
  };

  return typedPoxInfo;
};

const getStackingMinimum = () => {
  const response = simnet.callReadOnlyFn(
    POX_CONTRACT,
    "get-stacking-minimum",
    [],
    address1
  );
  return Number((response.result as UIntCV).value);
};

const burnHeightToRewardCycle = (burnHeight: number) => {
  const poxInfo = getPoxInfo();
  return Number(
    (BigInt(burnHeight) - poxInfo.firstBurnchainBlockHeight) /
      poxInfo.rewardCycleLength
  );
};

// Helper function to send a stacking transaction
const stackStx = (
  stacker: StackerInfo,
  amount: number,
  startBurnHeight: number,
  lockPeriod: number,
  maxAmount: number,
  authId: number,
  sender: string = address1
) => {
  const rewardCycle = burnHeightToRewardCycle(startBurnHeight);
  const sigArgs = {
    authId,
    maxAmount,
    rewardCycle,
    period: lockPeriod,
    topic: Pox4SignatureTopic.StackStx,
    poxAddress: stacker.btcAddr,
    signerPrivateKey: stacker.signerPrivKey,
  };
  const signerSignature = stacker.client.signPoxSignature(sigArgs);
  const signerKey = Cl.bufferFromHex(stacker.signerPubKey);

  const stackStxArgs = [
    Cl.uint(amount),
    poxAddressToTuple(stacker.btcAddr),
    Cl.uint(startBurnHeight),
    Cl.uint(lockPeriod),
    Cl.some(Cl.bufferFromHex(signerSignature)),
    signerKey,
    Cl.uint(maxAmount),
    Cl.uint(authId),
  ];

  return simnet.callPublicFn(POX_CONTRACT, "stack-stx", stackStxArgs, sender);
};

// Helper function to send a deletage transaction
const delegateStx = (
  amount: number,
  delegateTo: string,
  untilBurnHeight?: number,
  poxAddr?: string,
  sender: string = address1
) => {
  const delegateStxArgs = [
    Cl.uint(amount),
    Cl.principal(delegateTo),
    untilBurnHeight ? Cl.some(Cl.uint(untilBurnHeight)) : Cl.none(),
    poxAddr ? Cl.some(poxAddressToTuple(poxAddr)) : Cl.none(),
  ];

  return simnet.callPublicFn(
    POX_CONTRACT,
    "delegate-stx",
    delegateStxArgs,
    sender
  );
};

beforeEach(() => {
  simnet.setEpoch("3.0");
});

describe("test `set-burnchain-parameters`", () => {
  it("sets the parameters correctly", () => {
    const response = simnet.callPublicFn(
      POX_CONTRACT,
      "set-burnchain-parameters",
      [Cl.uint(100), Cl.uint(5), Cl.uint(20), Cl.uint(6)],
      address1
    );
    expect(response.result).toBeOk(Cl.bool(true));

    const fbbh = simnet.getDataVar(
      POX_CONTRACT,
      "first-burnchain-block-height"
    );
    expect(fbbh).toBeUint(100);

    const ppcl = simnet.getDataVar(POX_CONTRACT, "pox-prepare-cycle-length");
    expect(ppcl).toBeUint(5);

    const prcl = simnet.getDataVar(POX_CONTRACT, "pox-reward-cycle-length");
    expect(prcl).toBeUint(20);

    const configured = simnet.getDataVar(POX_CONTRACT, "configured");
    expect(configured).toBeBool(true);
  });

  it("cannot be called twice", () => {
    simnet.callPublicFn(
      POX_CONTRACT,
      "set-burnchain-parameters",
      [Cl.uint(100), Cl.uint(5), Cl.uint(20), Cl.uint(6)],
      address1
    );
    const response = simnet.callPublicFn(
      POX_CONTRACT,
      "set-burnchain-parameters",
      [Cl.uint(101), Cl.uint(6), Cl.uint(21), Cl.uint(7)],
      address1
    );
    expect(response.result).toBeErr(Cl.int(ERR_NOT_ALLOWED));
  });
});

describe("test `burn-height-to-reward-cycle`", () => {
  it("returns the correct reward cycle", () => {
    let response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(1)],
      address1
    );
    expect(response.result).toBeUint(0);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(2099)],
      address1
    );
    expect(response.result).toBeUint(1);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(2100)],
      address1
    );
    expect(response.result).toBeUint(2);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(2101)],
      address1
    );
    expect(response.result).toBeUint(2);
  });

  it("returns the correct reward cycle with modified configuration", () => {
    simnet.callPublicFn(
      POX_CONTRACT,
      "set-burnchain-parameters",
      [Cl.uint(100), Cl.uint(5), Cl.uint(20), Cl.uint(6)],
      address1
    );

    expect(() =>
      simnet.callReadOnlyFn(
        POX_CONTRACT,
        "burn-height-to-reward-cycle",
        [Cl.uint(1)],
        address1
      )
    ).toThrowError();

    expect(() =>
      simnet.callReadOnlyFn(
        POX_CONTRACT,
        "burn-height-to-reward-cycle",
        [Cl.uint(99)],
        address1
      )
    ).toThrowError();

    let response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(100)],
      address1
    );
    expect(response.result).toBeUint(0);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(101)],
      address1
    );
    expect(response.result).toBeUint(0);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(119)],
      address1
    );
    expect(response.result).toBeUint(0);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(120)],
      address1
    );
    expect(response.result).toBeUint(1);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(121)],
      address1
    );
    expect(response.result).toBeUint(1);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "burn-height-to-reward-cycle",
      [Cl.uint(140)],
      address1
    );
    expect(response.result).toBeUint(2);
  });
});

describe("test `reward-cycle-to-burn-height`", () => {
  it("returns the correct burn height", () => {
    let response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "reward-cycle-to-burn-height",
      [Cl.uint(0)],
      address1
    );
    expect(response.result).toBeUint(0);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "reward-cycle-to-burn-height",
      [Cl.uint(1)],
      address1
    );
    expect(response.result).toBeUint(1050);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "reward-cycle-to-burn-height",
      [Cl.uint(2)],
      address1
    );
    expect(response.result).toBeUint(2100);

    expect(() =>
      simnet.callReadOnlyFn(
        POX_CONTRACT,
        "reward-cycle-to-burn-height",
        [Cl.uint(340282366920938463463374607431768211455n)],
        address1
      )
    ).toThrowError();
  });
});

describe("test `current-pox-reward-cycle`", () => {
  it("returns the correct reward cycle", () => {
    let response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "current-pox-reward-cycle",
      [],
      address1
    );
    expect(response.result).toBeUint(0);

    simnet.mineEmptyBlocks(2099);

    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "current-pox-reward-cycle",
      [],
      address1
    );
    expect(response.result).toBeUint(1);

    simnet.mineEmptyBlock();
    response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "current-pox-reward-cycle",
      [],
      address1
    );
    expect(response.result).toBeUint(2);
  });
});

describe("test `get-stacker-info`", () => {
  it("returns none when principal is not stacked", () => {
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-stacker-info",
      [Cl.principal(address1)],
      address1
    );
    expect(response.result).toBeNone();
  });

  it("returns info before stacked", () => {
    const stacker = stackers[0];
    const amount = getStackingMinimum() * 1.2;
    let stackResponse = stackStx(
      stacker,
      amount,
      1000,
      6,
      amount,
      stacker.authId
    );
    expect(stackResponse.result.type).toBe(ClarityType.ResponseOk);
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-stacker-info",
      [Cl.principal(stacker.stxAddress)],
      address1
    );
    expect(response.result).toBeSome(
      Cl.tuple({
        "delegated-to": Cl.none(),
        "first-reward-cycle": Cl.uint(1),
        "lock-period": Cl.uint(6),
        "pox-addr": poxAddressToTuple(stacker.btcAddr),
        "reward-set-indexes": Cl.list([
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
        ]),
      })
    );
  });

  it("returns info while stacked", () => {
    const stacker = stackers[0];
    const amount = getStackingMinimum() * 1.2;
    let stackResponse = stackStx(
      stacker,
      amount,
      1000,
      6,
      amount,
      stacker.authId
    );
    expect(stackResponse.result.type).toBe(ClarityType.ResponseOk);
    simnet.mineEmptyBlocks(2100);
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-stacker-info",
      [Cl.principal(stacker.stxAddress)],
      address1
    );
    expect(response.result).toBeSome(
      Cl.tuple({
        "delegated-to": Cl.none(),
        "first-reward-cycle": Cl.uint(1),
        "lock-period": Cl.uint(6),
        "pox-addr": poxAddressToTuple(stacker.btcAddr),
        "reward-set-indexes": Cl.list([
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
          Cl.uint(0),
        ]),
      })
    );
  });

  it("returns none after stacking expired", () => {
    const stacker = stackers[0];
    const amount = getStackingMinimum() * 1.2;
    let stackResponse = stackStx(
      stacker,
      amount,
      1000,
      6,
      amount,
      stacker.authId
    );
    expect(stackResponse.result.type).toBe(ClarityType.ResponseOk);
    simnet.mineEmptyBlocks(7350);
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-stacker-info",
      [Cl.principal(stacker.stxAddress)],
      address1
    );
    expect(response.result).toBeNone();
  });
});

describe("test `check-caller-allowed`", () => {
  it("returns true when called directly", () => {
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-caller-allowed",
      [],
      address1
    );
    expect(response.result).toBeBool(true);
  });
});

describe("test `get-check-delegation`", () => {
  it("returns none when principal is not delegated", () => {
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-check-delegation",
      [Cl.principal(address1)],
      address1
    );
    expect(response.result).toBeNone();
  });

  it("returns info after delegation", () => {
    const amount = getStackingMinimum() * 1.2;

    const untilBurnHeight = 10;
    const delegateResponse = delegateStx(
      amount,
      address2,
      untilBurnHeight,
      stackers[0].btcAddr,
      address1
    );
    expect(delegateResponse.events).toHaveLength(1);
    expect(delegateResponse.result).toBeOk(Cl.bool(true));

    const delegateInfo = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-check-delegation",
      [Cl.principal(address1)],
      address1
    );
    expect(delegateInfo.result).toBeSome(
      Cl.tuple({
        "amount-ustx": Cl.uint(amount),
        "delegated-to": Cl.principal(
          "ST2CY5V39NHDPWSXMW9QDT3HC3GD6Q6XX4CFRK9AG"
        ),
        "pox-addr": Cl.some(poxAddressToTuple(stackers[0].btcAddr)),
        "until-burn-ht": Cl.some(Cl.uint(untilBurnHeight)),
      })
    );
  });

  it("does not expire if no burn height limit is set", () => {
    const amount = getStackingMinimum() * 1.2;

    delegateStx(amount, address2, undefined, stackers[0].btcAddr, address1);

    const delegateInfo = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-check-delegation",
      [Cl.principal(address1)],
      address1
    );

    simnet.mineEmptyBlocks(10_000);
    expect(delegateInfo.result).toBeSome(
      Cl.tuple({
        "amount-ustx": Cl.uint(amount),
        "delegated-to": Cl.principal(
          "ST2CY5V39NHDPWSXMW9QDT3HC3GD6Q6XX4CFRK9AG"
        ),
        "pox-addr": Cl.some(poxAddressToTuple(stackers[0].btcAddr)),
        "until-burn-ht": Cl.none(),
      })
    );
  });

  it("returns none after burn height expiration", () => {
    const amount = getStackingMinimum() * 1.2;
    simnet.mineEmptyBlock();

    const untilBurnHeight = 10;
    delegateStx(
      amount,
      address2,
      untilBurnHeight,
      stackers[0].btcAddr,
      address1
    );

    simnet.mineEmptyBlocks(2 + untilBurnHeight - simnet.blockHeight);
    // a stacks block height of 12 means a burnchain block height of 11
    assert(simnet.blockHeight === 12);

    const delegateInfo = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-check-delegation",
      [Cl.principal(address1)],
      address1
    );
    expect(delegateInfo.result).toBeNone();
  });
});

describe("test `get-reward-set-size`", () => {
  it("returns 0 when no stacking has occurred", () => {
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-size",
      [Cl.uint(0)],
      address1
    );
    expect(response.result).toBeUint(0);
  });

  it("returns number of stackers", () => {
    const amount = getStackingMinimum() * 1.2;

    stackers.forEach((stacker) => {
      const { result } = stackStx(
        stacker,
        amount,
        1000,
        6,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
      expect(result).toHaveClarityType(ClarityType.ResponseOk);
    });

    const responseCycle1 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-size",
      [Cl.uint(1)],
      address1
    );
    expect(responseCycle1.result).toBeUint(3);

    const responseCycle7 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-size",
      [Cl.uint(7)],
      address1
    );
    expect(responseCycle7.result).toBeUint(0);
  });

  it("returns number of uniq pox address", () => {
    const amount = getStackingMinimum() * 1.2;

    stackers.forEach((_stacker) => {
      const stacker: StackerInfo = {
        ..._stacker,
        btcAddr: stackers[0].btcAddr,
      };
      const { result } = stackStx(
        stacker,
        amount,
        1000,
        6,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
      expect(result).toHaveClarityType(ClarityType.ResponseOk);
    });

    const responseCycle1 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-size",
      [Cl.uint(1)],
      address1
    );
    expect(responseCycle1.result).toBeUint(3); // should it be 1?
  });
});

describe("test `get-total-ustx-stacked`", () => {
  it("returns 0 when no stacking has occurred", () => {
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(0)],
      address1
    );
    expect(response.result).toBeUint(0);
  });

  it("returns total amount stacked", () => {
    const amount = getStackingMinimum() * 1.2;

    stackers.forEach((stacker) => {
      const { result } = stackStx(
        stacker,
        amount,
        1000,
        6,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
      expect(result).toHaveClarityType(ClarityType.ResponseOk);
    });

    const responseCycle1 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(1)],
      address1
    );
    expect(responseCycle1.result).toBeUint(amount * 3);
  });

  it("returns 0 in the cycle before stacking starts", () => {
    const amount = getStackingMinimum() * 1.2;

    // stacking txs sent in cycle 0, so stackers will be start in cycle 1
    stackers.forEach((stacker) => {
      stackStx(
        stacker,
        amount,
        1000,
        6,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
    });

    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(0)],
      address1
    );
    expect(response.result).toBeUint(0);
  });

  it("returns total amount stacked", () => {
    const amount = getStackingMinimum() * 1.2;

    stackers.forEach((stacker) => {
      stackStx(
        stacker,
        amount,
        1000,
        6,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
    });

    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(1)],
      address1
    );
    expect(response.result).toBeUint(amount * 3);
  });

  it("expires stacking after the stacking duration has finsihed", () => {
    const amount = getStackingMinimum() * 1.2;

    stackers.forEach((stacker, i) => {
      const { result } = stackStx(
        stacker,
        amount,
        1000,
        // wallet_1 will expire after 2 cycles, wallet_2 after 4, wallet_3 after 6
        (i + 1) * 2,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
      expect(result).toHaveClarityType(ClarityType.ResponseOk);
    });

    const responseCycle3 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(3)],
      address1
    );
    expect(responseCycle3.result).toBeUint(amount * 2);

    const responseCycle5 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(5)],
      address1
    );
    expect(responseCycle5.result).toBeUint(amount * 1);

    const responseCycle7 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-total-ustx-stacked",
      [Cl.uint(7)],
      address1
    );
    expect(responseCycle7.result).toBeUint(0);
  });
});

describe("test `get-reward-set-pox-address`", () => {
  it("returns none when there is no stacker", () => {
    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-pox-address",
      [Cl.uint(0), Cl.uint(0)],
      address1
    );
    expect(result).toBeNone();
  });

  it("returns pox address for a stacker", () => {
    const amount = getStackingMinimum() * 1.2;
    stackers.forEach((stacker) => {
      stackStx(
        stacker,
        amount,
        1000,
        6,
        amount,
        stacker.authId,
        stacker.stxAddress
      );
    });

    const responseStacker0 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-pox-address",
      [Cl.uint(1), Cl.uint(0)],
      address1
    );
    expect(responseStacker0.result).toBeSome(
      Cl.tuple({
        "pox-addr": poxAddressToTuple(stackers[0].btcAddr),
        signer: Cl.bufferFromHex(stackers[0].signerPubKey),
        stacker: Cl.some(Cl.principal(stackers[0].stxAddress)),
        "total-ustx": Cl.uint(amount),
      })
    );
    const responseStacker1 = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-reward-set-pox-address",
      [Cl.uint(1), Cl.uint(1)],
      address1
    );
    expect(responseStacker1.result).toBeSome(
      Cl.tuple({
        "pox-addr": poxAddressToTuple(stackers[1].btcAddr),
        signer: Cl.bufferFromHex(stackers[1].signerPubKey),
        stacker: Cl.some(Cl.principal(stackers[1].stxAddress)),
        "total-ustx": Cl.uint(amount),
      })
    );
  });
});

describe("test `get-stacking-minimum`", () => {
  it("returns the correct minimum amount", () => {
    const response = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "get-stacking-minimum",
      [],
      address1
    );
    expect(response.result).toBeUint(125000000000);
  });
});

describe("test `check-pox-addr-version`", () => {
  it("returns true for a valid version", () => {
    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-pox-addr-version",
      [poxAddressToTuple(stackers[0].btcAddr).data.version],
      address1
    );
    expect(result).toBeBool(true);
  });

  it("returns false for an invalid version (> 6)", () => {
    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-pox-addr-version",
      [Cl.buffer(Buffer.from([7]))],
      address1
    );
    expect(result).toBeBool(false);
  });
});

describe("test `check-pox-addr-hashbytes`", () => {
  it("returns true for a valid address", () => {
    const segwitAddress = poxAddressToTuple(
      "36op6KLxdjBeBXnkNPi59UDTT2yZZGBYDm"
    );

    const segwitCheck = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-pox-addr-hashbytes",
      [segwitAddress.data.version, segwitAddress.data.hashbytes],
      address1
    );
    expect(segwitCheck.result).toBeBool(true);

    const taprootAddress = poxAddressToTuple(
      "bc1q82mfyran6u3y8r877vgkje45wlmvh85c7su3ljww9jv762znmrasn5ce59"
    );

    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-pox-addr-hashbytes",
      [taprootAddress.data.version, taprootAddress.data.hashbytes],
      address1
    );
    expect(result).toBeBool(true);
  });
});

describe("test `check-pox-lock-period`", () => {
  it("returns true for a valid lock period", () => {
    for (let i = 1; i <= 12; i++) {
      const { result } = simnet.callReadOnlyFn(
        POX_CONTRACT,
        "check-pox-lock-period",
        [Cl.uint(i)],
        address1
      );
      expect(result).toBeBool(true);
    }
  });

  it("returns false lock period of 0", () => {
    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-pox-lock-period",
      [Cl.uint(0)],
      address1
    );
    expect(result).toBeBool(false);
  });

  it("returns false lock period of 13", () => {
    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "check-pox-lock-period",
      [Cl.uint(13)],
      address1
    );
    expect(result).toBeBool(false);
  });
});

describe("test `can-stack-stx` and `minimal-can-stack-stx`", () => {
  it("returns true for a valid stacker", () => {
    const stacker = stackers[0];
    const amount = getStackingMinimum() * 1.2;
    const canStackArgs = [
      poxAddressToTuple(stacker.btcAddr),
      Cl.uint(amount),
      Cl.uint(1), // first reward cycle
      Cl.uint(6), // lock period
    ];

    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "can-stack-stx",
      canStackArgs,
      address1
    );
    expect(result).toBeOk(Cl.bool(true));
  });

  it("returns error if amount is too low", () => {
    const stacker = stackers[0];
    const amount = getStackingMinimum() / 2;
    const canStackArgs = [
      poxAddressToTuple(stacker.btcAddr),
      Cl.uint(amount),
      Cl.uint(1), // first reward cycle
      Cl.uint(6), // lock period
    ];

    const { result } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "can-stack-stx",
      canStackArgs,
      address1
    );
    expect(result).toBeErr(Cl.int(ERR_STACKING_THRESHOLD_NOT_MET));
  });

  it("returns error if period is too low or to high", () => {
    const stacker = stackers[0];
    const amount = getStackingMinimum() * 1.2;
    const canStackArgsTooLow = [
      poxAddressToTuple(stacker.btcAddr),
      Cl.uint(amount),
      Cl.uint(1), // first reward cycle
      Cl.uint(0), // lock period
    ];

    const { result: resultTooLow } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "can-stack-stx",
      canStackArgsTooLow,
      address1
    );
    expect(resultTooLow).toBeErr(Cl.int(ERR_STACKING_INVALID_LOCK_PERIOD));

    const canStackArgsTooHigh = [
      poxAddressToTuple(stacker.btcAddr),
      Cl.uint(amount),
      Cl.uint(1), // first reward cycle
      Cl.uint(13), // lock period
    ];

    const { result: resultTooHigh } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "can-stack-stx",
      canStackArgsTooHigh,
      address1
    );
    expect(resultTooHigh).toBeErr(Cl.int(ERR_STACKING_INVALID_LOCK_PERIOD));
  });

  it("returns error if pox address is invalid", () => {
    const addressTupleWrongVersion = Cl.tuple({
      hashbytes: Cl.buffer(
        Buffer.from("j89046x7zv6pm4n00qgqp505nvljnfp6xfznyw")
      ),
      version: Cl.buffer(Buffer.from([7])),
    });
    const amount = getStackingMinimum() * 1.2;
    const canStackArgs = [
      addressTupleWrongVersion,
      Cl.uint(amount),
      Cl.uint(1), // first reward cycle
      Cl.uint(6), // lock period
    ];
    const { result: resultWrongVersion } = simnet.callReadOnlyFn(
      POX_CONTRACT,
      "can-stack-stx",
      canStackArgs,
      address1
    );
    expect(resultWrongVersion).toBeErr(
      Cl.int(ERR_STACKING_INVALID_POX_ADDRESS)
    );
  });
});
