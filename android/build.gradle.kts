import org.gradle.api.tasks.wrapper.Wrapper

plugins {
    id("com.android.application") version "8.5.2" apply false
    id("org.jetbrains.kotlin.android") version "2.0.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.20" apply false
}

// Lets `gradle wrapper` (run with any system Gradle) emit gradlew + wrapper JAR
// pinned to a version AGP 8.5.x supports (requires Gradle 8.7+).
tasks.named<Wrapper>("wrapper") {
    gradleVersion = "8.9"
    distributionType = Wrapper.DistributionType.BIN
}
