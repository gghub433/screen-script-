import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Version: monotonic code from the commit count (or -PversionCode), name from version.properties.
val versionProps = Properties().apply { file("../version.properties").inputStream().use { load(it) } }
val vName = (findProperty("versionName") as String?) ?: versionProps.getProperty("VERSION_NAME")
val vCode = ((findProperty("versionCode") as String?) ?: versionProps.getProperty("VERSION_CODE")).toInt()
val updateRepo = (findProperty("revizor.updateRepo") as String?) ?: "gghub433/screen-script-"

// Release signing. The SAME key must sign every build, otherwise Android refuses in-place updates.
fun secret(name: String): String? = (findProperty(name) as String?) ?: System.getenv(name)
val ksPath = secret("REVIZOR_KEYSTORE")

android {
    namespace = "app.revizor"
    compileSdk = 34

    defaultConfig {
        applicationId = "app.revizor"
        minSdk = 29          // AudioPlaybackCapture + thermal API need Android 10
        targetSdk = 34
        versionCode = vCode
        versionName = vName
        buildConfigField("String", "UPDATE_REPO", "\"$updateRepo\"")
        // Limit with -Previzor.abis=arm64-v8a (e.g. when building on a phone).
        ndk { abiFilters += ((findProperty("revizor.abis") as String?)?.split(",") ?: listOf("arm64-v8a", "armeabi-v7a", "x86_64")) }
    }

    signingConfigs {
        if (ksPath != null) {
            create("release") {
                storeFile = file(ksPath)
                storePassword = secret("REVIZOR_KEYSTORE_PASSWORD")
                keyAlias = secret("REVIZOR_KEY_ALIAS") ?: "revizor"
                keyPassword = secret("REVIZOR_KEY_PASSWORD") ?: secret("REVIZOR_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = if (ksPath != null) signingConfigs.getByName("release") else signingConfigs.getByName("debug")
        }
        debug {
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    packaging { jniLibs { useLegacyPackaging = false } }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.09.00")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.6")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.6")
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.work:work-runtime-ktx:2.9.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
    debugImplementation("androidx.compose.ui:ui-tooling")
}
