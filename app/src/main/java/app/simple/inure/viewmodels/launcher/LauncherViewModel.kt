package app.simple.inure.viewmodels.launcher

import android.app.Application
import androidx.lifecycle.LiveData
import androidx.lifecycle.MutableLiveData
import app.simple.inure.extensions.viewmodels.WrappedViewModel

class LauncherViewModel(application: Application) : WrappedViewModel(application) {

    private val shouldVerify: MutableLiveData<Boolean> by lazy { MutableLiveData(false) }

    fun getShouldVerify(): LiveData<Boolean> {
        return shouldVerify
    }

    fun initCheck() {
        shouldVerify.postValue(false)
    }

    companion object {
        private const val TAG = "LauncherViewModel"
    }
}
