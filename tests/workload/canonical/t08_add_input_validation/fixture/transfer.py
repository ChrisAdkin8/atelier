"""Toy money-transfer module."""

ACCOUNTS = {"alice": 100, "bob": 50, "carol": 200}


def transfer(amount, from_acct, to_acct):
    ACCOUNTS[from_acct] -= amount
    ACCOUNTS[to_acct] += amount
    return ACCOUNTS[from_acct], ACCOUNTS[to_acct]
