import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Telegram API credentials (from https://my.telegram.org).
// Put the REAL values in android/local.properties (gitignored) or
// ~/.gradle/gradle.properties — never commit them. Placeholders in the
// checked-in gradle.properties stay commented out.
val localProps = Properties().apply {
    val f = rootProject.file("local.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}
val tdlibApiId = localProps.getProperty("tdlib.apiId")
    ?: (findProperty("tdlib.apiId") as String?)
    ?: "0"
val tdlibApiHash = localProps.getProperty("tdlib.apiHash")
    ?: (findProperty("tdlib.apiHash") as String?)
    ?: ""

android {
    namespace = "sh.coobyk.verity"
    compileSdk = 35

    defaultConfig {
        applicationId = "sh.coobyk.verity"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "1.0.0"

        buildConfigField("int", "TDLIB_API_ID", tdlibApiId)
        buildConfigField("String", "TDLIB_API_HASH", "\"$tdlibApiHash\"")
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    // TDLib is vendored — there is no trustworthy maven artifact:
    //   Java sources:  app/src/main/java/org/drinkless/tdlib/Client.java
    //                  app/src/main/java/org/drinkless/td/libcore/telegram/TdApi.java
    //   native libs:   app/src/main/jniLibs/<abi>/libtdjni.so
    // (copy both from a build of the td repo's example/android.)
    packaging {
        jniLibs.excludes += "META-INF/**"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.4")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.4")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")

    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")
    debugImplementation("androidx.compose.ui:ui-tooling")
}
