package io.github.hypercar.headunit

import org.junit.Assert.*
import org.junit.Test

class ReceiverCompatibilityTest {
    @Test fun desktopAudioCompatibilityRequiresDesktopProfileOrCurrentIdentity() {
        assertTrue(ReceiverCompatibility.isDesktopReceiver("winplay", null))
        assertTrue(ReceiverCompatibility.isDesktopReceiver("bmw_id85", "WinPlay receiver"))
        assertTrue(ReceiverCompatibility.isDesktopReceiver("bmw_id85", "DiPlay"))
        assertFalse(ReceiverCompatibility.isDesktopReceiver("bmw_id85", "BMW MGU"))
        assertFalse(ReceiverCompatibility.isDesktopReceiver("generic", null))
    }
    @Test fun desktopIdentityOverridesRetainedBmwProfile() {
        assertFalse(ReceiverCompatibility.useBmwBootstrap("bmw_id85", "DiPlay DiPlay DiPlay"))
        assertFalse(ReceiverCompatibility.useBmwBootstrap("bmw_id85", "WinPlay PC receiver"))
        assertFalse(ReceiverCompatibility.useBmwBootstrap("bmw_id85", "WINPLAY"))
    }
    @Test fun xpeng3ScreenSetupFollowsProfileIdNotJsonFields() {
        assertTrue(HeadUnitProfiles.usesXpeng3ScreenSetup("xpeng_g6"))
        assertFalse(HeadUnitProfiles.usesXpeng3ScreenSetup("bmw_id85"))
        assertFalse(HeadUnitProfiles.usesXpeng3ScreenSetup("generic"))
        assertFalse(HeadUnitProfiles.usesXpeng3ScreenSetup(null))
    }
    @Test fun bmwAndUnknownReceiversKeepSelectedBmwBehavior() {
        assertTrue(ReceiverCompatibility.useBmwBootstrap("bmw_id85", "BMW MGU"))
        assertTrue(ReceiverCompatibility.useBmwBootstrap("bmw_id85", null))
        assertFalse(ReceiverCompatibility.useBmwBootstrap("winplay", null))
        assertFalse(ReceiverCompatibility.useBmwBootstrap("generic", "BMW"))
    }
}
