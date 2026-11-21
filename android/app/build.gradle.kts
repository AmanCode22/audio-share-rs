import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "io.audiosharers.client"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.audiosharers.client"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "1.0.0"
    }

    signingConfigs {
        // CI injects a real keystore via env vars; local builds fall back to debug.
        // NOTE: do NOT declare a local `val storeFile` here — it shadows the AGP
        // property and `storeFile = storeFile` becomes a val-reassignment error.
        create("ciRelease") {
            storeFile = System.getenv("ANDROID_KEYSTORE_FILE")?.takeIf { it.isNotBlank() }?.let { file(it) }
            storePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD")
            keyAlias = System.getenv("ANDROID_KEY_ALIAS")
            keyPassword = System.getenv("ANDROID_KEY_PASSWORD")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // Signed with CI keystore when secrets are present; otherwise the auto-generated
            // debug key, so release APKs/AABs from CI are installable without secrets.
            signingConfig = if (!System.getenv("ANDROID_KEYSTORE_FILE").isNullOrBlank()) {
                signingConfigs.getByName("ciRelease")
            } else {
                signingConfigs.getByName("debug")
            }
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        compose = true
    }
}

// Migrated from the deprecated `kotlinOptions { jvmTarget = "17" }` DSL.
kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.service)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.datastore.preferences)
    implementation(libs.kotlinx.coroutines.android)
    debugImplementation(libs.androidx.compose.ui.tooling)
}
