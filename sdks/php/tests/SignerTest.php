<?php

declare(strict_types=1);

namespace Aeon\ProcessorSdk\Tests;

use Aeon\ProcessorSdk\Signer;
use PHPUnit\Framework\TestCase;

class SignerTest extends TestCase
{
    public function testGenerate(): void
    {
        $signer = Signer::generate();
        $this->assertSame(64, strlen($signer->publicKeyHex()));
        $this->assertStringStartsWith('ed25519:', $signer->publicKeyFormatted());
        $this->assertSame(64, strlen($signer->fingerprint()));
    }

    public function testFromSeedDeterministic(): void
    {
        $seed = str_repeat("\x42", 32);
        $s1 = Signer::fromSeed($seed);
        $s2 = Signer::fromSeed($seed);
        $this->assertSame($s1->publicKeyHex(), $s2->publicKeyHex());
    }

    public function testDifferentSeedsDifferentKeys(): void
    {
        $s1 = Signer::fromSeed(str_repeat("\x01", 32));
        $s2 = Signer::fromSeed(str_repeat("\x02", 32));
        $this->assertNotSame($s1->publicKeyHex(), $s2->publicKeyHex());
    }

    public function testSignChallenge(): void
    {
        $signer = Signer::generate();
        $nonceHex = str_repeat('deadbeef', 8);
        $sigHex = $signer->signChallenge($nonceHex);
        $this->assertSame(128, strlen($sigHex));
    }

    public function testSignAndVerifyBatch(): void
    {
        $signer = Signer::generate();
        $data = 'test batch data';
        $sig = $signer->signBatch($data);
        $this->assertSame(64, strlen($sig));
        $this->assertTrue($signer->verify($data, $sig));
    }

    public function testVerifyFailsWithWrongData(): void
    {
        $signer = Signer::generate();
        $sig = $signer->signBatch('original data');
        $this->assertFalse($signer->verify('tampered data', $sig));
    }

    public function testInvalidSeedLength(): void
    {
        $this->expectException(\InvalidArgumentException::class);
        Signer::fromSeed(str_repeat("\x00", 16));
    }
}
